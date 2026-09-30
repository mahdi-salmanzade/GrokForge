# AGENTS.md — GrokForge

GrokForge dogfoods this convention: the agent reads this file for project context. Keep it accurate.

## What this is

A Rust workspace implementing GrokForge, an open-source terminal coding agent for xAI Grok. Current implementation map: `docs/architecture.md`. Full design record lives in `docs/design/`; architecture decisions in `docs/decisions/`.

## Build / test / lint

GitHub Actions CI, nightly checks, and release publishing are disabled. Run these
checks locally. Workflow definitions are archived in `.github/disabled-workflows/`.

```sh
cargo build --workspace
cargo test --workspace                 # or: cargo nextest run --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo insta review                     # review pending TUI snapshot changes
cargo deny check                       # license / advisory gate
```

## Crate map

| Crate | Responsibility |
|---|---|
| `grokforge-protocol` | Serde-only shared vocabulary: `Op`/`Event`, approvals, `SandboxPolicy`, ledger, ids. Leaf crate, no tokio. **Append-only once released.** |
| `grokforge-config` | Typed Figment layering for owner/project config plus provider and model/runtime defaults. Credentials stay in the host binary. |
| `grokforge-xai` | In-house Grok client (`/v1/responses`), SSE streaming, model validation, request byte-accounting. |
| `grokforge-core` | Agent loop, tools, approval engine, attachments/memory, context assembler + redaction, compaction, sessions store, subagents. |
| `grokforge-sandbox` | `SandboxPolicy` compilation, per-OS backends, denial classifier, process exec. |
| `grokforge-git` | Bounded git-CLI reads and mutations from the trusted host process. gix-backed reads are deferred. |
| `grokforge-context` | Bounded repo map, file search, and secure local LSP/formatter configuration. |
| `grokforge-mcp` | Bounded stdio and Streamable HTTP MCP clients behind an internal trait. |
| `grokforge-render` | Pure-function bounded semantic Markdown rendering and terminal-text sanitization. |
| `grokforge-tui` | ratatui frontend. |
| `grokforge-server` | Authenticated loopback HTTP API, bounded SSE protocol stream, session metadata, OpenAPI. |
| `grokforge` | Binary: TUI + `exec` headless + subcommands. |
| `grokforge-test-support` | Bounded mock xAI HTTP/SSE server with captured requests and controllable TCP fragmentation. |

## Conventions

- **No `unwrap`/`expect` outside tests** (clippy-warned). Libraries error via `thiserror`; binaries use `color-eyre`/`anyhow`.
- **Protocol types are append-only** after a release — frontends and persistence depend on the wire format.
- **All git mutations run from the trusted host process**, never inside the sandbox. `.git` is deny-write in every sandbox policy.
- **The context ledger is the privacy guarantee.** Any code path that sends bytes to the network must go through the ledgered request path — never construct a raw request to the API.
- **Do not paste code from projects whose license is incompatible with MIT.** Learn from ideas and public interfaces, then implement the work independently.
- Agents run in `workspace-write` by default; commands are sandboxed with network off.

## Where to start

The spine is `grokforge-protocol` → `grokforge-xai` → `grokforge-core` (headless) before the TUI. See `docs/design/03-roadmap.md` for milestone order.
