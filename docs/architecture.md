# Current implementation map

This guide describes the implemented Rust workspace. The documents in `design/` preserve
the original proposal and roadmap; their crate counts, dependency choices, and planned
features do not always describe the current code. `Cargo.toml` and the source are the
authority for the running implementation.

## Workspace boundaries

There are 13 crates. All frontends reuse the same core agent and protocol vocabulary.

| Crate | Implemented responsibility | Start here |
|---|---|---|
| `grokforge` | Clap entry point, host credentials, startup configuration, TUI/headless/ACP/HTTP adapters, sessions, doctor, completions, diagnostics | [`main.rs`](../crates/grokforge/src/main.rs), [`headless.rs`](../crates/grokforge/src/headless.rs), [`acp.rs`](../crates/grokforge/src/acp.rs) |
| `grokforge-protocol` | Serde-only operations/events, IDs, transcript items, approvals/questions, sandbox policies, ledger and usage; no async runtime | [`lib.rs`](../crates/grokforge-protocol/src/lib.rs) |
| `grokforge-config` | Defaults, secure owner config, explicitly trusted project agent preferences, environment overrides; no credential persistence | [`lib.rs`](../crates/grokforge-config/src/lib.rs) |
| `grokforge-xai` | Grok Responses API, bounded request serialization, model catalog validation, SSE parsing, retry/timeout handling, subscription OAuth | [`client.rs`](../crates/grokforge-xai/src/client.rs), [`stream.rs`](../crates/grokforge-xai/src/stream.rs) |
| `grokforge-core` | Turn loop, approvals, tool registry, redaction/ledger assembly, attachments, instructions/skills/commands, compaction, sessions, isolated subagents | [`turn.rs`](../crates/grokforge-core/src/turn.rs), [`context.rs`](../crates/grokforge-core/src/context.rs), [`tools/mod.rs`](../crates/grokforge-core/src/tools/mod.rs) |
| `grokforge-sandbox` | Seatbelt/Bubblewrap policy compilation and execution, protected Git/credential paths, scrubbed environments, bounded output, cancellation and denial classification | [`lib.rs`](../crates/grokforge-sandbox/src/lib.rs), [`exec.rs`](../crates/grokforge-sandbox/src/exec.rs) |
| `grokforge-git` | Trusted-host Git CLI reads and mutations, worktrees, scoped commits and trailer attribution, isolated undo, foreground journal prototype | [`lib.rs`](../crates/grokforge-git/src/lib.rs) |
| `grokforge-context` | Bounded lexical repository maps, owner-controlled LSP/formatter configuration, one-shot LSP request/response parsing; core executes prepared commands | [`repo_map.rs`](../crates/grokforge-context/src/repo_map.rs), [`config.rs`](../crates/grokforge-context/src/config.rs), [`lsp_query.rs`](../crates/grokforge-context/src/lsp_query.rs) |
| `grokforge-mcp` | Bounded stdio and Streamable HTTP JSON-RPC clients, tool discovery, remote request-body accounting, MCP OAuth discovery and refresh | [`lib.rs`](../crates/grokforge-mcp/src/lib.rs), [`http.rs`](../crates/grokforge-mcp/src/http.rs), [`oauth.rs`](../crates/grokforge-mcp/src/oauth.rs) |
| `grokforge-render` | Pure bounded Markdown-to-semantic-lines/spans rendering, styles/links/block metadata, terminal-text sanitization | [`lib.rs`](../crates/grokforge-render/src/lib.rs) |
| `grokforge-tui` | Ratatui/crossterm application, composer/transcript/activity, approval/question queues, slash commands, usage and ledger views, terminal restoration | [`app.rs`](../crates/grokforge-tui/src/app.rs), [`lib.rs`](../crates/grokforge-tui/src/lib.rs) |
| `grokforge-server` | Authenticated loopback Axum API, session metadata, bounded prompt SSE streams, admission limits and workspace read/write gates | [`lib.rs`](../crates/grokforge-server/src/lib.rs), [API guide](../crates/grokforge-server/README.md) |
| `grokforge-test-support` | Mock xAI HTTP/SSE server with scripted responses, TCP fragmentation and exact captured request bodies/headers | [`mock.rs`](../crates/grokforge-test-support/src/mock.rs) |

## A turn through the system

1. The binary resolves the workspace, owner configuration, credentials and explicit trust
   flags, validates the model catalog, and prepares a durable session plus its tool registry.
   Configuration does not contain API keys. Project agent preferences require
   `--trust-project-config`; project MCP and executable tools have separate trust flags.
2. The frontend provides prompts, approval decisions and answers to the core `Agent`.
   `turn.rs` expands attachments and assembles instructions, history and tool definitions.
   `context.rs` redacts context, serializes the model request, reconciles ledger entries to
   its exact body size and enforces the input budget. Auxiliary compaction requests use
   the same accounting path.
3. `XaiClient` sends the configured request and parses bounded SSE into typed events.
   Initial connection retries are observable for ledger accounting. A failure after
   streaming starts is surfaced to the agent; the HTTP client does not silently replay
   already-delivered events. Redirects and ambient proxies are disabled.
4. The core gates tool calls through its approval policy. Host file operations enforce
   workspace/path rules themselves; command tools use the OS sandbox. Custom tools use
   hash-pinned private executable copies. Local LSP and formatter commands reuse the
   sandbox path. MCP tools cross a separately trusted external boundary.
5. Tool results are bounded and redacted before entering history. Protocol events feed
   frontend activity, usage and ledger views. Canonical history is appended to the rollout.
   The loop continues until completion, cancellation, failure or the iteration limit.
6. Compaction changes the model-visible history window while preserving the full rollout.
   Subagents use private Git worktrees outside the project, with a maximum of 32 per turn
   and no nested spawning. Their scoped changes remain available for explicit review/merge.

## Persistence and credentials

`core/store.rs` implements owner-private append-only JSONL rollouts, JSON metadata sidecars,
session locks and bounded list/search/resume/export/fork operations. The running session
store uses sidecar/transcript scans; the SQLite index proposed in ADR 0002 is not implemented.
Compaction checkpoints preserve the model-visible summary/tail on resume. Runtime plan state
is workspace-scoped and is not restored by conversation resume.

`grokforge/credentials.rs` owns the password-encrypted `~/.grokforge/credentials.enc` file.
Argon2id derives the key; ChaCha20-Poly1305 authenticates and encrypts API keys and OAuth
tokens. `XAI_API_KEY` provides the non-interactive override. The agent receives no credential
storage tool. Rollouts themselves are private files, not encrypted files; image attachments
remain encoded in the saved history. See [SECURITY.md](../SECURITY.md).

## Trust and concurrency boundaries

- The model ledger accounts for serialized request bodies and redactions. Remote MCP has
  separate JSON-RPC body accounting. HTTP headers, responses, OAuth control traffic and
  external-process activity are outside those byte totals.
- Git operations execute on the trusted host, with hardened executable/environment handling
  and repository hooks/helper overrides. Git metadata remains protected inside command
  sandboxes. Git reads currently use the CLI too; gix-backed reads are deferred.
- Native macOS uses Seatbelt; Linux uses validated Bubblewrap. Unsupported enforcement fails
  closed for normally sandboxed commands. Native Windows confinement remains unimplemented.
- HTTP prompt admission and event queues are bounded. Execute prompts hold an exclusive
  workspace gate through setup and completion; plan prompts share a read gate. Disconnects
  request cooperative cancellation, while admission permits stay held until the runner exits.
  These gates coordinate this server process, not external editors or other processes.
- Ordinary foreground/shared-workspace sessions and HTTP sessions do not auto-commit edits.
  Isolated subagent worktrees provide the commit-attributed workflow. The foreground journal
  prototype is not enabled because it cannot attribute concurrent external saves safely.

## Verification and automation

GitHub Actions CI, nightly checks and release publishing are disabled. Definitions and
restoration instructions are preserved in [`.github/disabled-workflows/`](../.github/disabled-workflows/README.md).
Run build, tests, Clippy, formatting and cargo-deny locally as described in
[CONTRIBUTING.md](../CONTRIBUTING.md). The source privacy audit is part of the test suite,
including without GitHub Actions.

Mock integration tests exercise Responses streaming, tool/approval loops, ledger
reconciliation and the HTTP API without billable model calls. Platform sandbox tests and
the [manual terminal matrix](testing/terminal-matrix.md) cover behavior that a source-level
review cannot establish. The design's dedicated PTY harness, full diff/scrollback pipeline,
automatic semantic context selection and signed installers remain deferred.
