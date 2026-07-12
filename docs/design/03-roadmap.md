# GrokForge v0.1 Implementation Roadmap

Project decisions: full differentiator scope, a Grok-only in-house client, OS-native sandboxing in v0.1, the MIT License, and the `grokforge` binary.

---

## 0. Critique of the Proposed Sequence

The example order (scaffold → client → headless agent → TUI → approvals → sandbox → git → repo map → MCP → sessions → plan mode → polish) is broadly right. Five corrections:

1. **The sandbox *seam* cannot wait until the sandbox milestone.** The exec tool must spawn every command through a `SandboxPolicy`-shaped call path from the first day it exists (M2), with a passthrough backend. Retrofitting the seam after the TUI/approvals depend on raw `tokio::process::Command` is the classic rework trap. Real backends land later (M5); the type lands at M2.
2. **The approval *engine* is core logic, not TUI.** The 4-policy × 3-mode matrix from brief §3 must be decided headlessly in M2 (headless mode auto-denies or fails fast per policy); M4 only adds the modal UI. Splitting these makes the matrix unit-testable without a terminal.
3. **Session *recording* moves early; session *management* stays late.** Append-only JSONL rollout logs from M2 are the single best debugging and test-fixture tool for the agent loop (and what codex does). Resume/search/SQLite index remains a late milestone (M8).
4. **Privacy plumbing is absent from the example order but is the marketing wedge (brief §4.1).** Secret redaction must exist the moment file contents can leave the machine (M2). The request-audit data plane belongs in the client (M1). The context-ledger TUI panel lands with the context engine (M7). If this is bolted on at the end, "airtight" is unachievable and HN finds the gap.
5. **CI, insta snapshots, and the terminal matrix are M0/M3 concerns, not polish** — the brief's risk #9 mitigation says "from day one." MCP is order-independent and should float in a parallel lane; putting it serially after sessions was arbitrary.

**Dogfood gate:** after M4, GrokForge builds GrokForge. Every subsequent milestone is developed using the tool itself (with `AGENTS.md` in the repo — decision-locked convention dogfooding).

---

## 1. Milestones

Three lanes. **Lane A** is the serial product spine. **Lane B** (security) and **Lane C** (context/ecosystem) fork after M2 and are independently landable — a solo dev does them in the numbered order; 2–3 contributors run lanes concurrently.

```
M0 → M1 → M2 ─┬─→ M3 → M4 ─┬─→ M6 → M10 → M11   (Lane A, serial)
              ├─→ M5 (sandbox backends; UI escalation integrates after M4)   (Lane B)
              └─→ M7, M8, M9 (any order; M7's ledger panel needs M3)         (Lane C)
M10 requires M2+M5+M6 (+M9 if subagents get MCP tools). M11 requires everything.
```

### M0 — Repo scaffold, licensing, CI skeleton (S)

**Deliverables**
- Cargo workspace (layout in §2), all crates stubbed with `lib.rs` and one trivial test each; `crates/grokforge` binary prints version.
- `LICENSE`, `license = "MIT"` in workspace `Cargo.toml`; `deny.toml` (cargo-deny) forbidding incompatible or unknown dependency licenses.
- `README.md` (positioning, §2), `AGENTS.md` (dogfood, §2), `CONTRIBUTING.md`, `rustfmt.toml`, workspace `[workspace.lints]`, `.gitignore`.
- `.github/workflows/ci.yml`: 3-OS matrix, fmt + `clippy -D warnings` + test + cargo-deny (§3).
- Checklist item executed: "Grok" trademark exposure check (brief §4) — record outcome in a `docs/decisions/0001-name.md` ADR.
- Version-cliff verification (risk #7): lock ratatui 0.30.2 / crossterm 0.29 / reqwest 0.13.4 / eventsource-stream 0.2.3 / rmcp 2.2 in workspace deps now; confirm every planned widget dep declares `ratatui ^0.30`.

**Exit criteria**
- `cargo build --workspace && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo deny check` green in CI on ubuntu-24.04, macos-15, windows-latest.
- `grokforge --version` and `grokforge --help` (clap) work on all three.

### M1 — xAI client + streaming + mock SSE server (M)

**Deliverables** (`grokforge-xai` — Grok-only, concrete types, no provider trait; base URL and model IDs are `Config` fields, never constants)
- `POST /v1/responses` request/response serde types: input items, client tool defs (≤200), `parallel_tool_calls`, `response_format: json_schema`, reasoning effort (`low/medium/high/xhigh`), server-side tool defs (`web_search`, `x_search`, code-execution — probe live for the type string, remote MCP `{type:'mcp', server_url}`), `x-grok-conv-id` header.
- SSE streaming via `eventsource-stream` over `reqwest::bytes_stream` with in-house reconnect/backoff (do **not** use reqwest-eventsource); typed event enum including `reasoning_summary_text.delta`; tool calls parsed as whole-chunk (per brief — no partial-JSON accumulator needed).
- Usage extraction incl. `usage.input_tokens_details.cached_tokens`; separately-metered server-side tool call counting (for the cost display); error taxonomy (auth/429+Retry-After/5xx/stream-abort) with retry policy.
- Startup `GET /v1/models` validation: warn loudly on unknown/retired slug (silent redirect+re-price hazard).
- **Request-audit data plane**: every outgoing request emits a structured `RequestAudit` (byte counts per input item, file provenance tags) — the context ledger's substrate.
- `grokforge-test-support`: in-process mock xAI SSE server (hyper/axum) with scriptable transcripts — the fixture reused by every later milestone.
- Hidden `grokforge debug api "prompt"` command for live smoke testing.

**Exit criteria**
- `cargo test -p grokforge-xai`: fixture-driven tests for happy-path stream, event split across TCP chunks, keepalives, mid-stream disconnect + backoff resume, whole-chunk tool call, usage/cached-token extraction, 429 retry.
- With `XAI_API_KEY` set: `grokforge debug api "say hi"` streams live tokens; startup model validation warns when config lists a bogus slug.

### M2 — Agent core, tools, approval engine, headless exec (L)

**Deliverables** (`grokforge-core` + `grokforge-protocol`)
- Turn loop: submit → stream → dispatch tool calls (parallel-capable) → append results → continue until final; conversation state with token budgeting/truncation; stable message prefixes for prompt caching.
- Client tools: `read_file`, `apply_patch` (diffy-based, the write primitive), `exec` (through the `SandboxedCommand` seam — `SandboxPolicy` struct defined **now** in `grokforge-sandbox` with a passthrough backend), `grep`/`glob`/`list` (ignore+globset).
- **Approval engine (headless)**: policy matrix (untrusted/on-request/on-failure/never × read-only/workspace-write/danger-full-access) as a pure decision table; escalation request/response types in `grokforge-protocol`.
- **Redaction engine**: `.env`/secret-pattern redaction applied at the tool-output → conversation boundary, on by default.
- `AGENTS.md` discovery (repo root + nested dirs) merged into system prompt — dogfooding starts here.
- Reasoning-effort routing knob per request (plan=high, edit=low defaults; model IDs from config: `grok-build-0.1` default loop, `grok-4.5` plan, `grok-4.3` long-context, per brief §2).
- Session **recording**: size-capped append-only JSONL rollout per session under `~/.local/share/grokforge/sessions/` (rotation math unit-tested — the anti-640TB/yr guard).
- Headless CLI: `grokforge exec -p "..."` with `--json` (JSON-lines protocol events on stdout) and `--full-auto` / `--sandbox <mode>` flags. **GrokForge is usable for real work from this milestone.**

**Exit criteria**
- Integration tests against the mock SSE server: multi-turn tool loop, parallel tool calls, denial→feedback loop, truncation behavior; golden JSONL of protocol events (insta).
- In a fixture repo: `grokforge exec -p "create hello.py printing hi" --full-auto` creates the file (live or mock-backed).
- Decision-table unit tests cover all 12 policy×mode cells; redaction test proves a planted `.env` value never appears in captured request bodies.
- Rollout log rotation test caps disk usage.

### M3 — TUI shell + streaming render pipeline (XL)

Two internal tracks, parallelizable: (a) shell/input, (b) render pipeline.

**Deliverables** (`grokforge-tui`)
- Hybrid inline viewport: `Viewport::Inline`, history committed to native scrollback via `scrolling-regions`/`insert_before`; every draw in DEC 2026 synchronized output; no full-screen `Clear`; `alternate_screen = auto|always|never` with Zellij detection + raw-write path; alt-screen only for transcript overlay.
- Custom composer widget: grapheme-cluster cursors (unicode-segmentation), unicode-width, per-width wrap cache, kill ring, input history, bracketed paste + burst heuristic; nucleo+ignore `@`-file popup; slash-command popup (incl. `/model` mid-session switch — risk #3 hook).
- Streaming markdown: pulldown-cmark stable-queue + mutable live tail; adaptive commit ticks (queue depth + staleness); hold back tables and unclosed fences; URL lines unwrapped; per-cell wrap cache keyed (content, width, theme); syntect highlight cache keyed (language, code); collapsible reasoning cell fed by `reasoning_summary_text.delta`.
- Status line: token/cost meter incl. cached-token savings and server-side tool call meter; minimal ledger readout (bytes sent this request) ahead of the full M7 panel.
- Theme: OSC 10/11 in raw mode with timeout + COLORFGBG/WT_SESSION fallbacks + config override; 3-tier color ladder.
- Scriptable TUI test harness: feed synthetic events into the app against ratatui `TestBackend`, snapshot frames with insta.

**Exit criteria**
- `cargo insta test -p grokforge-tui` snapshot suite (80/120 cols; truecolor/256/16) green on all OSes.
- Perf guard test: render a 10k-line transcript, assert per-frame time bounded and byte-identical committed cells (no O(n²) re-wrap — the goose/crush bug).
- Manual: `grokforge` inside tmux and Zellij — native scrollback intact after streaming, no OSC leakage, paste of multi-line text intact (checklist script from §4 run once here, full matrix in CI from now on).

### M4 — Approvals + diff UX (M)

**Deliverables**
- Bottom-pane approval modal (not a screen): ordered options (proceed / don't-ask-again-this-session / deny / deny-with-feedback), single-key shortcuts, syntax-highlighted `$ command` preview, inline diff for patches, fullscreen diff/approval escape hatch; the modal reserves an **"escalate & retry"** option slot (wired live in M5).
- Diff rendering: similar hunks, syntect state preserved per hunk, line-number gutter + sign column, DIM deleted lines, ~10k-line highlight cap, 3-tier palette.
- `grokforge-git` (reads only, gix): status/diff context for the approval flow and dirty-worktree awareness.
- **Dogfood gate**: team switches to GrokForge for GrokForge development.

**Exit criteria**
- insta snapshots: modal states, diffs at all three color tiers.
- Mock-driven e2e: agent proposes patch → `y` applies, file changes; `d`+feedback returns the message to the model (assert in the JSONL rollout).
- `grokforge exec` and TUI both honor identical decisions from the shared M2 engine (one parameterized test).

### M5 — Sandbox v0.1, all three OS strategies (L) — Lane B, forkable after M2

**Deliverables** (`grokforge-sandbox`)
- `SandboxPolicy` (writable/readable roots, unreadable globs, protected metadata, network `Isolated|ProxyRouted|Full` — `ProxyRouted` present in the enum but returns "unsupported in v0.1"; Phase-2 hook).
- **Linux**: in-process landlock 0.4.5 `ABI::V5` BestEffort (read-only `/`, writable roots + `/dev/null`) + seccompiler network-deny (EPERM on connect/bind/listen/sendto except AF_UNIX; hard-block `io_uring_setup/enter/register`, ptrace, `process_vm_*`) + `PR_SET_NO_NEW_PRIVS`; `RulesetStatus` surfaced to the TUI status line — never silently degrade. Kernel floor 5.13.
- **macOS**: SBPL generated from policy, exec'd via hardcoded `/usr/bin/sandbox-exec -p <policy> -D PARAM=path` (paths only ever as `-D` params — injection guard is a unit test); SBPL base policy **reimplemented from scratch** against the observed behavior of codex's seatbelt policy (license decision 4 — study, don't copy); startup self-test (`sandbox-exec -n no-network true`) with graceful approval-only fallback.
- **Windows**: WSL2 delegation (Linux backend inside WSL); native fallback = sandbox-off + explicit per-command approval; capability honestly reported in `grokforge doctor`/README.
- Denial classifier (sandbox-denied vs. real failure) + one-keystroke escalate-and-retry wired into the M4 modal; default-deny egress in workspace-write from day one.
- `grokforge debug sandbox -- <cmd>` subcommand.
- Escape smoke-test suite (§4).

**Exit criteria**
- On ubuntu-22.04 **and** ubuntu-24.04 runners (ABI variance): escape suite passes — write outside roots → EPERM, TCP/UDP/DNS connect → EPERM, `io_uring_setup` → blocked, ptrace → blocked, `.git` write → denied; `RulesetStatus` reported matches kernel ABI.
- On macos-14/15 runners: `grokforge debug sandbox -- curl https://example.com` fails classified as sandbox denial; write-outside-root denied; self-test fallback path covered by forcing a failure.
- TUI: denied command shows classifier verdict + escalation key; escalation re-runs unsandboxed after approval.

### M6 — Git-native workflow (M)

**Deliverables** (`grokforge-git` mutations; all git mutations execute from the **trusted host process**, never inside the sandbox; `.git` stays deny-write inside — brief §3's decided tension)
- Auto-commit per agent edit with structured-output commit messages (json_schema — brief §2); configurable off.
- `/undo` = git revert of agent commits; dirty-worktree guard on session start; branch mode (`grokforge/<task>` working branch) + PR-ready summary generation.
- Worktree management primitives (create/list/remove) — the substrate for M10 subagents.
- gix for reads, `git` CLI shell-out for mutations (gix push/commit unverified — brief §1).

**Exit criteria**
- Fixture-repo e2e: task run yields one descriptive commit per edit; `/undo` reverts cleanly; `git log` messages conform to schema.
- Sandbox cross-test: agent-spawned command attempting `.git/hooks` write is denied while the host commit succeeds in the same session.
- `grokforge worktree add/remove` round-trips.

### M7 — Repo map + context engine + context ledger UI (M) — Lane C

**Deliverables** (`grokforge-context`)
- Tree-sitter repo map (Aider-style ranked symbol signatures, grammars pinned on the LanguageFn ABI), text-splitter (`code` feature) chunking, token-budgeted map injection into the system prompt; `.gitignore`/`.grokforgeignore` respected. **No embeddings** — semantic index is deferred (user decision 5); keyword+structure retrieval only.
- **Context ledger TUI panel**: per-request list of exactly which files/byte counts left the machine (from M1's `RequestAudit`), redaction indicators, cumulative session totals. This must reconcile byte-for-byte with actual request bodies — it is the trust feature.
- `grokforge debug repomap` prints the ranked map.

**Exit criteria**
- Fixture repo: `grokforge debug repomap --budget 2000` output stable (insta) and under budget.
- Ledger reconciliation test: instrumented mock server compares received body sizes to ledger claims — exact match required.
- Redaction e2e: planted secrets in `.env` and inline in source comments never appear in mock-server-received bodies.

### M8 — Sessions + persistence (M) — Lane C

**Deliverables**
- rusqlite index over the M2 JSONL rollouts (list/search); `grokforge resume` (latest / `--last` / picker), session forking; resume preserves stable prefix + `x-grok-conv-id` for cache hits; vision caveat honored (stateless requests when images in context).
- Secrets: keyring v4 (platform stores + encrypted-file fallback for headless Linux) in `spawn_blocking`; `grokforge login` stores the API key.

**Exit criteria**
- Kill TUI mid-turn → `grokforge resume` restores transcript and continues the turn.
- `grokforge sessions list` and search work; sqlite index rebuilds from JSONL if deleted.
- Headless-Linux CI job (no DBus) exercises the encrypted-file keyring fallback.

### M9 — MCP + plugins/skills conventions (M) — Lane C

**Deliverables** (`grokforge-mcp`)
- rmcp 2.2 **behind an internal trait** (risk #7 isolation; this is an infra seam, not the forbidden provider abstraction), stdio + streamable-HTTP clients, spec 2025-11-25, deprecated sampling/roots/logging avoided; server config in `config.toml`; tool namespacing folded into the ≤200-tool request budget; MCP tool calls flow through the same approval engine and appear in the context ledger.
- xAI Docs MCP (`https://docs.x.ai/api/mcp`) as optional built-in; server-side remote-MCP passthrough (`{type:'mcp'}`) exposed distinctly from client-side MCP.
- Custom slash commands from `.grokforge/commands/*.md`; skills convention (`.grokforge/skills/<name>/SKILL.md`) discovered and injected; documented in AGENTS.md-compatible terms.

**Exit criteria**
- Integration test: spawn a reference MCP server (stdio), list tools, invoke one through the full approval path, result reaches the model (mock SSE asserts).
- A repo-local slash command and skill are discovered and used in an e2e run; GrokForge's own repo gains a dogfood skill.

### M10 — Plan mode + subagents (L)

**Deliverables**
- Plan mode: read-only enforcement **reuses the sandbox read-only mode** (not honor-system), plan generated by config-routed flagship model (`grok-4.5`, effort high) with json_schema plan structure; plan review/accept UI → execution handoff to the default loop model.
- Subagents: ≤8 parallel workers, each in its own git worktree (M6 primitives) with its own sandbox instance and rollout log; orchestrator merge-back UX (diff review per worktree); RPS throttling for tier-gated multi-agent model (`grok-4.20-multi-agent-0309`, 9 RPS at Tier 0).

**Exit criteria**
- Plan mode run: sandbox test asserts zero writes occurred during planning; plan renders as a reviewable checklist; accept executes it.
- Fixture-repo demo: 3 parallel subagents on disjoint tasks produce 3 worktrees, merged back with per-worktree approval; total RAM under budget during the run.

### M11 — Hardening, docs, release (L)

**Deliverables**
- cargo-dist 0.32 release pipeline: shell + PowerShell installers, Homebrew tap (`grokforge/homebrew-tap`), cargo-binstall metadata; targets x86_64/aarch64 for linux-gnu(+musl if painless) and macOS, x86_64-windows.
- macOS Developer ID signing + notarization — recommend **rcodesign** in CI (runs on Linux runners, notarizes via App Store Connect API key) over runner-local codesign; Gatekeeper verification step. Windows Authenticode explicitly out of scope for v0.1 (documented).
- Privacy audit as a **test**: full-session run against an egress-recording proxy asserts the only remote host contacted is the configured base URL; zero telemetry code paths exist.
- Benchmarks published in README: RSS < 100 MB, cold start, vs. opencode/grok-cli (brief §4.4).
- Full manual terminal-matrix pass (§4 script), CHANGELOG + semver policy, docs, `grokforge doctor` (sandbox capability report per OS), issue templates.

**Exit criteria**
- Tagged `v0.1.0` → CI produces installers; `curl | sh` install works on clean Linux/macOS VMs, `irm | iex` on Windows; `brew install grokforge/tap/grokforge` works; `cargo binstall grokforge` works.
- `spctl -a -vv` passes on the notarized macOS artifact.
- Egress-audit test green; benchmark numbers in README reproduce within tolerance.

**Rough size ledger:** S×1, M×6, L×4, XL×1. Longest serial path: M0→M1→M2→M3→M4→M6→M10→M11.

---

## 2. Repo Scaffolding Specifics (M0)

**Workspace layout**
```
GrokForge/
├── Cargo.toml                 # workspace; [workspace.lints]; license = "MIT"
├── LICENSE
├── README.md, AGENTS.md, CONTRIBUTING.md, CHANGELOG.md
├── rustfmt.toml, deny.toml, .gitignore
├── .github/workflows/{ci.yml,release.yml,nightly.yml}
├── docs/decisions/            # ADRs (0001-name/trademark, 0002-grok-only-client, 0003-sandbox-tiers)
├── docs/testing/terminal-matrix.md
├── assets/grokforge.svg       # existing icon moves here
└── crates/
    ├── grokforge/             # bin crate → binary "grokforge"; clap subcommands (exec, resume, sessions, login, doctor, debug)
    ├── grokforge-core/        # agent loop, tools, approval engine, redaction, prompt assembly
    ├── grokforge-protocol/    # serde event/op types shared by all frontends  ← the ACP hook
    ├── grokforge-xai/         # in-house /v1/responses client (user decision 2: own crate, concrete types)
    ├── grokforge-tui/         # ratatui frontend (composer, markdown pipeline, ledger panel)
    ├── grokforge-sandbox/     # SandboxPolicy + linux/macos/windows backends + denial classifier
    ├── grokforge-git/         # gix reads, git-CLI mutations, worktrees
    ├── grokforge-context/     # repo map, chunking, file search
    ├── grokforge-mcp/         # rmcp behind internal trait
    └── grokforge-test-support/# mock xAI SSE server, fixture repos, TUI/PTY harness
```
`grokforge-session` starts as a `core` module and splits out at M8 only if dependency pressure demands it.

- **README positioning**: “Make Grok great in the terminal.” Explain what works, state platform limits plainly, and keep roadmap items clearly separate from shipped features.
- **Licensing**: MIT. CONTRIBUTING requires original or MIT-compatible contributions; cargo-deny enforces dependency policy.
- **AGENTS.md** (dogfood from M2): build/test commands (`cargo test --workspace`, `cargo insta review`, `cargo clippy -- -D warnings`), crate map + ownership boundaries, conventions (no `unwrap` outside tests — clippy-enforced; errors via thiserror in libs, color-eyre in bins; protocol types are append-only once released), sandbox note ("agents run workspace-write; git mutations via the host").
- **rustfmt.toml**: stable-only options (edition 2024 in Cargo.toml, default style + `newline_style = "Unix"`). Skip nightly-only options (codex uses them; not worth forcing nightly on contributors).
- **Lints** (workspace `Cargo.toml`): `[workspace.lints.rust] unsafe_code = "deny"` except `grokforge-sandbox` (opts out locally — landlock/seccomp need it); `[workspace.lints.clippy] unwrap_used = "warn"`, `expect_used = "warn"`, `pedantic = { level = "warn", priority = -1 }` with targeted allows.
- **.gitignore**: `/target`, `*.pending-snap`, `.env`, `.grokforge/` local state, `.DS_Store`, `dist/` (committed `.snap` files stay in-tree).

---

## 3. CI/CD

**`ci.yml` (per PR)**
- Matrix: `ubuntu-22.04` (kernel 5.15, Landlock ABI≈V1–V2), `ubuntu-24.04` (6.8, ABI V4), `macos-14` + `macos-15` (arm64; SBPL drift coverage across OS versions — risk #4), `windows-latest`. Swatinem/rust-cache.
- Steps: fmt check → `clippy --all-targets -- -D warnings` → `cargo nextest run --workspace` → `cargo deny check` → insta with `INSTA_UPDATE=no` (fails on pending snapshots).
- **Sandbox job**: escape smoke suite on both Ubuntu kernels (asserting per-ABI `RulesetStatus`) and both macOS runners (Seatbelt self-test + denials). Windows job asserts the approval-only fallback path and WSL2 detection logic (unit-level; real WSL2 e2e is a documented manual/self-hosted check).
- **Terminal-matrix job** (Linux): install tmux/Zellij/screen; run the TUI under each via the PTY harness, capture frames (`tmux capture-pane -e`), diff against goldens; assert no OSC 10/11 reply leakage into the input stream and scrollback integrity after `insert_before`. Per-PR for tmux; Zellij/screen/SSH-loopback in `nightly.yml` (heavier, flakier).
- **Nightly**: live-API smoke (`XAI_API_KEY` secret; `#[ignore]`d tests) — catches xAI drift (risk #1) incl. model-slug validation and the code-execution type-string probe; full multiplexer matrix; headless-keyring job.

**`release.yml`** — cargo-dist generated, plus a post-build step: rcodesign sign + notarize macOS artifacts (Developer ID cert + App Store Connect key in secrets), `spctl` verify, then publish installers, Homebrew tap PR, binstall-compatible artifact names. Watch item: cargo-dist cadence post-axo-shutdown (brief §1) — pin the version, budget for a fork/migration in Phase 2 if needed.

---

## 4. Testing Strategy per Layer

| Layer | What | How / assertions |
|---|---|---|
| Unit | SSE chunker/parser | Fixture transcripts: events split mid-chunk, CRLF vs LF, keepalive comments, reconnect resume points |
| Unit | Markdown stable-queue | **Property test**: once a region is committed to scrollback it is never re-emitted differently; tables/unclosed fences held back; URL lines unwrapped |
| Unit | Policy compiler | `SandboxPolicy` → landlock ruleset descriptions and **generated SBPL text as insta string snapshots** (catches injection: path with `"` or `(` must appear only as `-D` param, never in policy text) |
| Unit | Approval matrix | All 12 cells of the decision table; headless vs TUI parity |
| Unit | Redaction, wrap cache, log rotation, commit-message schema | Planted secrets; width-change invalidation; size caps |
| Snapshot (insta) | TUI frames via `TestBackend` | 80/120 cols × truecolor/256/16; composer states, approval modal, diffs, reasoning cell collapsed/expanded, resize replay |
| Integration | Mock-SSE agent loop | Multi-turn tool runs, parallel tool calls, disconnect/resume, 429, deny-with-feedback; golden protocol-event JSONL; ledger-vs-received-bytes reconciliation |
| Sandbox smoke | Real kernels/OSes | Child process attempts: write outside roots, read unreadable glob, TCP/UDP/DNS egress, `io_uring_setup`, ptrace, `process_vm_readv`, `.git` write → assert EPERM/denial + correct classifier verdict; assert `RulesetStatus` matches kernel ABI (risk #8: never claim enforcement that isn't active) |
| Privacy audit | Egress-recording proxy | Full session run: only configured base URL contacted; zero telemetry (M11 gate, re-run every release) |
| Manual | `docs/testing/terminal-matrix.md` | tmux/Zellij/screen/SSH/Windows Terminal/iTerm2/kitty/Ghostty: multi-line paste, emoji width, hyperlinks, scrollback after streaming, resize storm, theme-detection timeout, alt-screen auto-off under Zellij. Run at M3, M4, and M11 |

---

## 5. Explicitly Deferred Past v0.1 + Reserved Hooks

| Deferred (brief Phase 2/3) | Hook reserved in v0.1 so no rearchitecting |
|---|---|
| bwrap namespaces (pid/net, tmpfs masking) | `SandboxBackend` selection enum + `sandbox.backend = "auto"` config key with reserved values; backends already compile one `SandboxPolicy` |
| L7 domain-allowlist proxy | `NetworkMode::ProxyRouted` variant exists (errors "unsupported"); `[network] allowed_domains` config key reserved; denial classifier already distinguishes network denials |
| Windows native restricted tokens / elevated dual-user | `sandbox.windows.native = false` reserved; Windows backend is already a strategy behind the same trait |
| Semantic index (fastembed + usearch) | `grokforge-context` keeps retrieval behind an internal module boundary; `[index] semantic = false` reserved; `~/.local/share/grokforge/index/` path reserved; ort-rc pin risk stays out of v0.1 |
| Multi-provider abstraction (user decision 2) | `grokforge-xai` crate boundary; client construction confined to one factory fn in core; config namespaced `[provider.grok] base_url/models` (not flat keys) so `[provider.*]` can be added without breaking configs |
| ACP editor embedding | `grokforge-protocol` crate: all core↔frontend events serde-serializable from day one; headless `--json` already proves the frontend seam; ACP = new frontend crate later |
| Voice, Telegram remote, image gen | No structural hooks needed beyond the protocol crate and data-driven tool registry; deliberately zero code reserved |
| Container fallback (`GROKFORGE_SANDBOX=docker`) | Env var name reserved and documented as unimplemented |

---

## 6. Risk Table → Retiring Milestone

| Brief risk | Retired (or bounded) at | Mechanism |
|---|---|---|
| 1. xAI platform churn | **M1** (bounded) | Base URL + model IDs as config; `/v1/models` startup validation; nightly live-API smoke. Residual: full multi-model hedge deferred by user decision 2 — the crate seam + `[provider.grok]` namespace is the insurance |
| 2. xAI squeezes independents | **M0/M11** (positioning only) | Open license, BYO-key, verifiable privacy; accepted residual until multi-provider ships post-v0.1 |
| 3. Model quality ceiling | **M3/M10** | `/model` mid-session switch (M3); per-role routing plan=grok-4.5 (M10); config-driven IDs |
| 4. sandbox-exec deprecation | **M5** | Startup self-test + graceful approval-only fallback; macos-14+15 CI drift coverage |
| 5. Linux userns restrictions | **By design (v0.1)** | v0.1 uses namespace-free Landlock+seccomp only; becomes live at Phase-2 bwrap |
| 6. Windows sandboxing hard | **M5 + M11** | WSL2 delegation + honest capability table in README/`doctor`; never marketed ahead of shipped |
| 7. Ecosystem version cliffs | **M0 + M3 + M9** | Dep pinning + deny check (M0); own composer/markdown renderer (M3); rmcp behind trait (M9) |
| 8. Silent security degradation | **M5** | Landlock always paired with seccomp net filter; io_uring/ptrace explicit blocks; `RulesetStatus` surfaced; per-ABI CI assertions |
| 9. TUI perf/compat landmines | **M3** | Stable-queue architecture, wrap/highlight caches, perf guard test, insta + multiplexer CI from M3 onward |
| 10. Privacy reputational blast radius | **M2 + M7 + M11** | Redaction (M2), byte-exact ledger reconciliation test (M7), egress-audit test + signed/notarized binaries + opt-in-only telemetry enforced in review (M11) |

---

### Critical Files for Implementation
- `Cargo.toml` — workspace metadata, lints, and dependency pins.
- `crates/grokforge-xai/src/lib.rs` — in-house Responses API client.
- `crates/grokforge-core/src/turn.rs` — turn loop, tool dispatch, and approvals.
- `crates/grokforge-sandbox/src/lib.rs` — sandbox policy and backend boundary.
- `.github/workflows/ci.yml` — cross-platform checks.
