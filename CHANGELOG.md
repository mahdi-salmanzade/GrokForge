# Changelog

All notable changes to GrokForge are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.1] - 2026-07-15

### Changed
- The Up/Down arrows now recall previously entered prompts (shell-style input history); the
  transcript scrolls with PageUp/PageDown and End.
- The live activity indicator moved from the bottom-left status bar to a one-line band directly
  above the composer (Claude Code style), so the animated braille "working…/thinking…/<tool>…"
  feedback appears where you're typing. The bottom line now carries persistent metadata only
  (model, preset, tokens, sandbox, branch).

### Fixed
- Long context-heavy sessions no longer dead-end on the input budget. When a request still exceeds
  the model's window after compaction (a big auto-loaded `AGENTS.md` plus large recent tool reads),
  GrokForge now shrinks the oldest tool outputs to fit and keeps going instead of aborting the turn
  — pairing is preserved and the full transcript stays in the rollout. Only a truly irreducible
  request stops with an actionable message.

### Fixed
- Raised the context-budget estimate from a worst-case 1 byte/token to a conservative 3 (~700 KB
  for a 256k-token model). The 1-byte budget could be *smaller than the auto-loaded `AGENTS.md` cap
  itself*, so even a trivial turn overran the budget with nothing to compact. The pre-send guard
  still measures the exact serialized request body against the budget.

## [1.0.0] - 2026-07-15

### Added
- Real typed configuration with defaults, a securely opened owner-controlled
  `~/.grokforge/config.toml`, an explicit `--trust-project-config` gate for the restricted project
  `.grokforge/config.toml`, and `GROKFORGE_CONFIG_*` environment overrides. Repository config can
  tune model/runtime preferences only after opt-in and cannot redirect the credential-bearing
  provider endpoint or weaken sandbox/approval policy. The TUI, headless, resume, and doctor paths
  now share these defaults.
- Global `--model` and `--effort` overrides (including explicit `auto` and `xhigh`), plus `/model`
  and `/effort` in the Forge Deck. `auto` clears a lower-precedence effort setting and selects the
  provider default. The startup model catalog is fetched once, reused for context-window sizing,
  and used to reject incompatible model/effort combinations before a request is sent. Successful
  switches persist both settings for resume, while older session metadata remains compatible.
  Interactive `/plan` turns temporarily use the configured plan model, its catalog context window,
  and high effort, then restore the normal session settings even when the turn fails. Model catalog
  UI data is no longer copied into every subagent session.
- Real `grokforge completions` output for Bash, Zsh, Fish, Elvish, and PowerShell.
- ACP (Agent Client Protocol) support via `grokforge acp`, so editors like Zed can embed GrokForge
  as an agent. It speaks JSON-RPC 2.0 over newline-delimited stdio as an additive frontend over the
  same `Op`/`Event` seam the headless frontend uses (fulfilling ADR 0005's hedge without a core
  rearchitecture). Protocol version 1: `initialize`, `session/new`, `session/prompt` with streaming
  `session/update` notifications and a `stopReason` response, `session/cancel`, and
  `session/request_permission` bridged from the approval engine so the editor gates
  boundary-crossing actions. Credentials come from `XAI_API_KEY` (stdin is the protocol channel);
  the official ACP v1 stdio `mcpServers` declarations are validated, bounded, started with a
  scrubbed environment, and registered atomically. Session persistence, `session/load`, remote MCP
  transports, and client `fs`/`terminal` calls are deferred.
- `@`-mention file and folder attachments. Typing `@path` in a prompt inlines that file (or the
  files under that folder) into the message as bounded `<attachment>` blocks, so it flows through
  the same redaction, ledger, and context-budget path as any other input — attaching a huge folder
  can no longer blow the prompt limit. Reads reuse the descriptor-relative, no-follow workspace
  reader (an `@path` cannot follow a symlink out of the workspace) and skip common secret files by
  default. The TUI adds an interactive fuzzy picker: type `@`, arrow-select a `.gitignore`-aware
  path suggestion, and Tab/Enter inserts it. `@`-expansion works in headless `exec` too.
  Quoted mentions such as `@"docs/design notes.md"` support paths containing spaces and escaped
  quotes without weakening the existing no-follow and size limits.
- Agent-managed persistent memory under `.grokforge/memory/`. A new `remember` tool lets Grok save
  durable facts, preferences, and project conventions across sessions; notes without a topic go to
  the `MEMORY.md` index, and topic notes go to a slugged `<topic>.md` file linked from the index.
  The `MEMORY.md` index is auto-loaded into context each session (redacted and ledgered like other
  auto-context), while topic bodies stay local until read. Writes are confined to the memory
  directory with a sanitized slug so a note can never escape it, and `/memory` shows the current
  memory in the TUI.

### Fixed
- Closed check/read races for trusted project config by opening `.grokforge/config.toml` relative
  to a no-follow directory descriptor and rejecting symlinks, hardlinks, and non-regular files.
- Fixed the `@` picker so paths containing spaces, quotes, or backslashes are inserted with the
  quoted/escaped syntax the attachment parser expects; terminal-hostile control-character paths
  are no longer offered in the palette.
- Hardened editor-supplied MCP against oversized discovery metadata, non-object tool schemas,
  unsupported task-required tools, provider-name collisions after redaction, secret-bearing tool
  metadata, unbounded ACP input lines, capability-negotiation mistakes, and missing protocol
  `ping` replies.
  Runtime MCP failures now use protocol-appropriate ACP error codes instead of the reserved
  authentication-required code.
- Hardened approval-free memory writes against symlink and hardlink redirection in every parent,
  final target, and legacy temporary-file position by reusing the descriptor-relative atomic
  workspace writer. Unsupported platforms fail closed.
- Hardened ACP session concurrency and cancellation ownership, bounded prompt/embedded-resource
  input to 512 KiB, required canonical absolute workspaces, and added permission-response expiry so
  disconnected clients cannot leave an approval hung forever.
- Bounded the assembled request to the model's real context window so a long session no longer
  overruns the provider's prompt-token limit and fails the whole turn with a hard `400`. The
  compaction trigger is now capped at an absolute, model-derived ceiling (the previous
  `baseline + trigger` threshold grew every compaction, letting history creep past the limit), and
  a pre-send guard checks the exact serialized body size: if it still exceeds the budget it forces
  one compaction and, failing that, stops with an actionable message instead of a raw provider
  error. The budget uses the model's advertised context window from `GET /v1/models` (with a
  conservative fallback) with a one-byte-per-token admission bound. Compaction now measures its
  full serialized auxiliary request against the same budget before any provider egress.

## [0.2.0] - 2026-07-14

### Changed
- Replaced OS-keychain credential storage with one password-encrypted file at
  `~/.grokforge/credentials.enc`. On first interactive use, GrokForge asks the user to set and
  confirm a password, then choose subscription OAuth or an xAI API key; later runs unlock the file
  with that password. The key is derived with Argon2id and a fresh random salt, and the credential
  payload is encrypted and authenticated with ChaCha20-Poly1305 and a fresh nonce (`0600`
  permissions on Unix). Wrong passwords or altered ciphertext are rejected. Expired OAuth tokens
  are refreshed and re-sealed with the same password, while `XAI_API_KEY` continues to override
  stored credentials without prompting so non-interactive CI remains practical. This removes all
  credential-flow use of platform secret stores, specifically to eliminate recurring macOS
  Keychain-access prompts and make storage and unlock behavior explicit and consistent; the
  corresponding tradeoff is that a forgotten password requires deleting the file and signing in
  again.
- Subagents now fan out in parallel and the per-turn cap was raised from 8 to 32. When the model
  requests several `spawn_task` calls in one response, they run concurrently — each in its own git
  worktree with a fresh sibling agent (depth cap 1). Admission (the approval gate and worktree
  creation) stays serialized so approvals remain ordered and concurrent `git worktree add` calls
  cannot race the repository ref locks; the subagent turns then execute at the same time and their
  results are recorded back into the parent transcript in the original call order. The interactive
  TUI gains a live "PARALLEL AGENTS" panel — one animated row per lane showing its status, current
  activity, and token use. Subagent events are tagged per lane so up to 32 concurrent streams no
  longer interleave into one transcript, while their token and privacy accounting still folds into
  the global totals.

### Fixed
- Codebase audit hardening. OAuth token exchange and refresh now use a timeout-bounded HTTP client
  that refuses redirects and environment proxies, bounds response bodies, validates token and
  expiry fields, and propagates body-read failures instead of masking them as an empty body; the
  loopback callback reader enforces an absolute deadline and an exact request-head size cap.
  Tool-call arguments—including provider-native function-call history—are redacted before they
  re-enter the model request body, with the redactions accounted in the context ledger so byte
  reconciliation stays exact. Git filter neutralization also disables a repository-configured
  `smudge` driver's `required` flag for defense in depth. The bearer- and basic-auth redaction
  patterns gained word-boundary anchors. On Unix the credentials directory is created owner-only
  (`0700`), passwords are held in memory that is zeroized on drop, and the derived key and decrypted
  credential bytes are scrubbed on every exit path. The TUI launch error path is terminal-sanitized.
- Hardened the encrypted credential file so Unix writes are created owner-only and atomically
  replace the previous file instead of writing first and applying `0600` permissions afterward.
  Unsupported, malformed, oversized, or billing-ambiguous envelopes are rejected; Unix also
  rejects linked paths and files readable by group or other users. Argon2id parameters are pinned
  for the v1 format, and failure to discover the home directory no longer falls back to writing in
  the current project. The native macOS/Linux sandbox masks the file from model-run commands even
  in full-access mode. This closes the permission window and blocks model-command reads of the
  encrypted secret. GrokForge recommends longer passwords and warns below 12 characters, but
  accepts any non-empty password instead of enforcing a minimum chosen on the user's behalf.
- Made API-key and subscription logins mutually exclusive. Choosing one now clears the other, so a
  successful subscription sign-in cannot accidentally continue using and billing a previously
  stored API key. Refresh persistence failures are also surfaced instead of silently discarded.
- Refined the local OAuth callback page with accurate pre-save wording, compact responsive sizing,
  distinct success/denial/waiting guidance, accessible non-wrapping keyboard hints, and factual
  local-only privacy copy. The page now sends users back to the terminal for the authoritative save
  result instead of claiming setup is complete before encrypted persistence finishes.
- Fixed the launch identity screen disappearing whenever a dirty workspace disabled auto-commit.
  Startup safety notices now remain visible without being mistaken for conversation history, so the
  bundled ASCII mark still renders until the first real prompt. The parallel-agent panel also fits
  narrow terminals and measures wide Unicode labels by terminal-cell width instead of character
  count.
- Queued simultaneous subagent approvals in arrival order. A later request can no longer replace
  an unresolved approval and silently deny it; quitting aborts both the visible request and every
  queued request so no agent remains blocked on a hidden prompt.

### Added
- A complete GrokForge visual refresh: an adaptive branded TUI with a calm high-contrast palette,
  compact welcome state, human-readable tool and Git activity, live reasoning/retry/usage/privacy
  status, semantic Markdown rendering, discoverable capabilities, and a responsive approval sheet
  that keeps the safe denial action visible on narrow terminals.
- A self-contained branded OAuth callback experience for success, cancellation, and waiting states.
  Callback state is validated before accepting a code, success is shown only after token exchange,
  and all terminal-facing authentication errors are sanitized to one line.
- Local project workflows: bounded, deterministic discovery of
  `.grokforge/skills/*/SKILL.md` guidance and `.grokforge/commands/*.md` slash commands. Skill
  descriptions are catalogued up front while full instructions remain local until Grok reads the
  selected file through the normal ledgered tool path.
- Safe read-only `git_status` and `git_diff` tools run from the trusted host Git boundary, with
  repository confinement, output caps, and protections against symlink, hard-link, filter, and
  text-conversion surprises.
- Explicit, default-off Grok-hosted web search, X search, and code interpreter tools. Headless runs
  use `--web-search`, `--x-search`, and `--code-interpreter`; the TUI exposes `/tools` status and
  per-session toggles.
- M0 scaffold: Cargo workspace with 12 crates, MIT licensing,
  `cargo-deny` license gate, CI matrix (Linux/macOS/Windows), and the design record
  under `docs/design/` and `docs/decisions/`.
- M1 xAI client (`grokforge-xai`): typed `POST /v1/responses` request model, SSE streaming
  via `eventsource-stream` with frame reassembly across TCP reads, typed events (text and
  reasoning deltas, whole-chunk tool calls with a partial-JSON accumulation hedge, usage with
  cached/reasoning-token breakdown), error taxonomy with initial-request retry/backoff
  (honoring `Retry-After`), `GET /v1/models` validation, and request byte-accounting for
  ledger reconciliation. Byte-controllable mock SSE server in `grokforge-test-support`; hidden
  `grokforge debug api` live smoke command.
- M2 agent core (`grokforge-core`, `grokforge-protocol`, `grokforge-sandbox`): the full
  headless agent loop. Protocol vocabulary (Op/Event, approvals, SandboxPolicy, ledger,
  items). Turn state machine (assemble → stream → run tools → loop). Built-in tools
  (read_file, write_file, edit, shell, list, glob, grep) behind a unified `Tool` trait, run
  through a `SandboxRunner` seam (passthrough backend; OS-native enforcement lands at M5).
  Pure 4×3 approval decision table (all 12 cells tested) with a deny-and-continue headless
  approver. Secret redaction at ingress + secrets.deny blocked-glob reads. Context assembler
  as the single ledger choke point, byte-reconciled with the serialized request. Append-only
  JSONL rollout persistence with size-capped debug-log rotation math. Working
  `grokforge exec -p` headless command (`--preset`, `--allow`, `--json`, `--model`,
  `--effort`, `--cd`) with CI-friendly exit codes.
- M3 (first cut) interactive TUI (`grokforge-tui`): a working ratatui frontend driving the
  agent over the same event/approval channels as headless. Async event loop (crossterm
  `EventStream` + agent events + approval requests in `tokio::select!`), scrolling transcript
  with role-styled entries and live streaming, composer, status line, and an interactive
  approval modal (`y`/`a`/`d`). `TestBackend` render tests. Launched by the default,
  no-subcommand `grokforge` invocation. (Uses the alternate screen; the inline-viewport +
  native-scrollback render pipeline from the design docs is the planned upgrade.)
- M5 OS-native sandbox (`grokforge-sandbox`): real kernel-enforced backends replacing the
  passthrough placeholder. macOS Seatbelt (`sandbox-exec` with a generated, param-passed SBPL
  profile — canonicalized paths, injection-guarded) confines writes to the workspace, keeps
  `.git` read-only, and denies network in workspace-write mode. Linux bubblewrap (`bwrap`
  shell-out: read-only root, workspace bind-mounted, network namespace unshared). Denial
  classifier (distinguishes sandbox blocks from real failures). `default_runner()` factory
  selects per platform and reports capability honestly (`enforced` flag). Wired into both
  frontends. Verified end-to-end: under `--preset auto` the model's write outside the
  workspace is blocked by the kernel.
- M6 git-native workflow (`grokforge-git` + core wiring): mutating turns in GrokForge-owned
  isolated worktrees become real commits from the trusted host process (never in the sandbox).
  Foreground/shared-worktree edits remain uncommitted until a race-safe edit journal lands. `Git`
  shells out to the `git` CLI for discovery/status/commit/undo/worktrees. Auto-commit stages
  **only the paths the
  agent touched** (a `TurnContext` collector fed by write_file/edit), attaches
  `Grokforge-Session`/`Grokforge-Turn` trailers, and neutralizes hooks (`--no-verify` + empty
  `hooksPath`). `undo_last` walks the session's contiguous trailer commits (`reset --keep` at the
  tip, else `revert`) and stops at foreign commits. Worktree add/remove primitives for M10.
  New `Committed` event surfaced in both frontends. Verified end-to-end: a write in a git repo
  produces a trailered `grokforge: update greet.py` commit.
- M8 (core) sessions & resume: rollouts + `SessionMeta` sidecars are now persisted under the
  XDG data dir by both frontends (full-UUID filenames). `grokforge sessions` lists saved
  sessions (id, model, workspace, first prompt); `grokforge resume [id]` reloads a transcript
  (`Session::with_history`) and reopens it in the TUI, continuing from prior history. Rollouts
  thread through interactive turns so ongoing sessions keep persisting. Verified end-to-end: a
  headless run is listed by `grokforge sessions`.
- M7 (compaction) context management: history is compacted at turn end once it exceeds a
  configurable byte threshold. The model writes a narrative summary while file paths (from
  write/edit tool calls) and error text (from failed tool results) are extracted
  **mechanically** and preserved verbatim — never paraphrased. The full transcript stays in the
  rollout; only the model-visible window shrinks. Pure functions unit-tested; the model-backed
  loop verified with a mock. (Repo map and the ledger TUI panel from M7 are deferred to a later
  pass.)
- M5.5 plan mode: `Agent::run_plan_turn` (headless `--plan`, TUI `/plan <task>`) runs a turn with
  read-only tools + a read-only sandbox + a planning preamble, so the agent produces a plan
  without changing anything. Enforced, not honor-system.
- **Security fix:** file-writing tools (`write_file`/`edit`) now enforce the sandbox policy's
  writable boundary in the host process. Previously they used `fs::write` directly, bypassing the
  OS sandbox (which only confines shelled-out commands) — so a write outside the workspace, or any
  write in plan/read-only mode, could slip through. Now refused with a `FsWrite` denial. New tests
  cover both cases.
- TUI slash commands: `/help`, `/plan <task>`, `/undo` (host-side Git undo for isolated-worktree
  session commits; foreground undo is not implemented), `/clear`, `/quit`.
- **Sandbox correctness fixes (important):** (1) the Seatbelt self-test used an invalid SBPL
  operation (`process-exec*`), so `available()` always failed and every run silently fell back to
  the unenforced passthrough runner — Seatbelt is now correctly detected and active. (2)
  `CommandSpec::shell` split the command on whitespace, mangling any quoted/piped/redirected
  command; it now runs through `/bin/sh -c` (`cmd /C` on Windows) so commands execute as written,
  under the sandbox. Verified end-to-end: a real `echo > /tmp/…` shell escape under `--preset auto`
  is now blocked by the kernel and classified as a sandbox denial.
- M11 (release readiness): `grokforge doctor` reports toolchain, the **actual** sandbox backend and
  whether it's enforced, git availability, and endpoint/telemetry status. `SECURITY.md` (privacy
  claim, what leaves the machine, per-platform sandboxing, threat model). Release workflow building
  per-target binaries on tag. macOS artifacts remain unsigned; signing/notarization,
  package-manager publishing, and a Homebrew tap are not implemented yet.
- M9 MCP (Model Context Protocol): `grokforge-mcp` is a minimal, hand-rolled JSON-RPC 2.0 stdio
  client behind an internal `McpConnection` trait (chosen over pinning an unverified `rmcp`
  version). `initialize` handshake, `tools/list`, `tools/call`. Core `McpToolAdapter` exposes each
  server tool as a `mcp__<server>__<tool>` GrokForge tool that is **always approval-gated** (its
  side effects are outside our sandbox). After the explicit `--trust-project-mcp` opt-in, a
  `.grokforge/mcp.json` loader connects declared servers at session start and registers their
  tools; wired into both frontends and resume. The default refuses to execute project config.
  Verified end-to-end: a configured mock MCP server's tool is called through the full agent loop.
- M10 subagents: a `spawn_task` tool (intercepted by the runtime) runs a self-contained subtask
  in an **isolated git worktree** on a `gf/agent/<id>` branch, via a fresh sibling agent with
  subagent-spawning disabled (depth cap 1). The subagent's edits auto-commit in the worktree; the
  parent receives the final result plus a `git diff --stat` summary and the branch name for
  manual review/merge (no auto-merge yet). The async-recursion Send cycle is broken with a
  boxed-`+ Send` return type on `spawn_subagent`. Verified end-to-end: a delegated subtask writes
  and commits a file on its own branch, visible to the parent.
