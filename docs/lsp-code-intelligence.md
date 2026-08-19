# Local LSP and formatter tools

GrokForge has three model-callable local code-intelligence tools:

- `lsp_diagnostics` opens one bounded text document and collects its latest published diagnostics.
- `lsp_query` supports `hover`, `definition`, `references`, `document_symbols`,
  `workspace_symbols`, and `implementation`.
- `format_file` runs a formatter against a private copy, rechecks the expected source immediately
  before installation, and refuses a detected intervening change. The final swap is atomic, but
  this is conflict detection rather than a portable cross-process compare-and-swap; a narrow
  same-user check-to-rename race remains.

No tool downloads a language server, starts a daemon, or uses the network. Built-in commands are
resolved from the owner's executable path; custom commands come only from the owner-private
`~/.grokforge/code-intelligence.toml`. The executable and its canonical target are checked again
immediately before every spawn. Arguments are exact argv values and never pass through a shell.

## `lsp_query`

Every call launches one server in the read-only OS sandbox with network disabled. GrokForge sends
bounded, `Content-Length` framed messages in this order:

1. `initialize`
2. `initialized`
3. `textDocument/didOpen` for the requested file
4. one read-only query
5. `textDocument/didClose`
6. `shutdown`
7. `exit`

The capture-style sandbox runner writes this lifecycle once, waits for the configured bounded
grace period, closes stdin, and reaps the process. This is deliberately not a persistent LSP
session and does not retain an index between calls.

Position-based operations take one-based `line` and one-based UTF-16 `character` values. Both are
validated against the bounded UTF-8 source before the server starts. `workspace_symbols` instead
requires a non-empty query of at most 512 bytes. The source/session cap is 1 MiB/2 MiB, captured
server output is capped at 64 KiB, at most 200 locations or symbols and 16 symbol-nesting levels
are retained, hover text is capped at 12 KiB, and final tool text is capped at 32 KiB.

Location, LocationLink, Hover, SymbolInformation, WorkspaceSymbol, and hierarchical
DocumentSymbol responses are normalized. Every returned URI must be a local `file:` URI naming an
existing, non-secret regular file inside the active workspace; malformed, duplicate, other-scheme,
secret, missing, and outside-workspace results are omitted. Names and hover text have terminal
control bytes removed. File URIs and absolute paths embedded in hover prose are withheld. Raw LSP
stdout and stderr are never echoed on parse or process failure.

Active `secrets.deny` globs are copied into the one-shot sandbox policy. The query process cannot
write anywhere, cannot use IP networking, and is tied to turn cancellation and the configured
process timeout. GrokForge proactively sends only the requested document over LSP, but initialize
also names the workspace root and the installed language server can inspect other read-only
filesystem paths visible inside the OS sandbox. Common host credentials and configured secret
paths are masked, returned locations are workspace-confined, and output is redacted, but the
language server is still locally installed code that must be trusted not to encode unrelated host
data into an otherwise valid result.

## Deliberate gaps

This one-shot implementation does not yet provide completion, rename, code actions, semantic
tokens, type/call hierarchy, or a long-lived capability-negotiated language-server session.
Definitions and references are useful now, but repeated calls pay server startup/indexing cost.
