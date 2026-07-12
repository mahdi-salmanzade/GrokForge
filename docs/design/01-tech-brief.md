# GrokForge — Tech Decision Brief

Synthesized from 5 research reports: **[xai-api]**, **[rust-stack]**, **[sandbox]**, **[competitors]**, **[tui-patterns]**. Dated 2026-07-12.

---

## 1. Recommended Stack

| Area | Choice | Version | Rationale |
|---|---|---|---|
| TUI framework | ratatui (+ ratatui-core for custom widgets) | 0.30.2 / core 0.1.x | Post-modularization; `scrolling-regions` feature enables flicker-free inline scrollback; leapfrogs codex-rs's patched 0.29 fork [rust-stack][tui-patterns] |
| Terminal backend | crossterm, `event-stream` + `bracketed-paste` features | 0.29.0 | EventStream + `tokio::select!` is the standard async loop; note upstream is ~15 mo stale [rust-stack] |
| Async runtime | tokio | 1.52.x | Uncontested [rust-stack] |
| HTTP client | reqwest | 0.13.4 | Current; breaks reqwest-eventsource — see next row [rust-stack] |
| SSE parsing | eventsource-stream (own reconnect/backoff) | 0.2.3 | Transport-agnostic, works over reqwest 0.13 `bytes_stream`; codex-validated. **Do not** use reqwest-eventsource (pinned to reqwest ^0.12) [rust-stack] |
| xAI client | **In-house thin client** against `/v1/responses` | n/a | No official Rust SDK; community crates (xai-sdk 0.10.0, ~1k downloads) are immature. rust-genai as reference only [xai-api][rust-stack] |
| Git | gix for reads (status/diff/log/blame/worktrees); shell out to `git` CLI for mutations | 0.85.0 | codex/jujutsu-validated pattern; avoids libgit2 C dep; gix push support unverified [rust-stack] |
| MCP | rmcp (official SDK), behind an internal trait | 2.2.0 | Client+server, stdio + streamable HTTP, spec 2025-11-25; leapfrogs codex (rmcp 1.8). Avoid deprecated sampling/roots/logging (SEP-2577) [rust-stack] |
| Markdown render | **Custom pulldown-cmark streaming renderer** (codex pattern) | pulldown-cmark 0.13 | Needed for the stable-queue/live-tail streaming pipeline; tui-markdown 0.3.8 is the runner-up but 0.x/incomplete [tui-patterns] — *conflict with [rust-stack]'s tui-markdown pick; resolved in favor of the streaming architecture requirement* |
| Syntax highlight | syntect (fancy-regex backend) + two-face | 5.3.0 / 0.5.1 | Pure-Rust regex = painless cross-compile; two-face gives bat-grade language coverage [rust-stack] |
| Diffs | similar + diffy | 2.7.0 / 0.4–0.5 | codex-validated pair [rust-stack] |
| Multi-line input | **Custom composer widget** (grapheme-aware, wrap cache, kill ring, @-mention elements) | n/a | tui-textarea is dead (Oct 2024) and incompatible with ratatui 0.30 — flagged consistently by [rust-stack][tui-patterns][competitors]. Runner-up: edtui 0.11.3 |
| Fuzzy file search | nucleo + ignore/globset | latest | codex file-search pattern for @-mentions [tui-patterns][rust-stack] |
| Code parsing/chunking | tree-sitter + text-splitter (`code` feature) | 0.26.11 / 0.32.0 | Repo map (Aider's crown) + chunking; pin grammars on the LanguageFn ABI [rust-stack] |
| Semantic index (post-MVP) | fastembed + usearch | 5.17.2 / 2.26.0 | Accept the ort rc pin; avoid sqlite-vec (upstream stalled) and lancedb (Arrow/DataFusion bloat) [rust-stack] |
| Sandbox (Linux) | landlock + seccompiler | 0.4.5 / 0.5.0 | Only maintained, production-proven pair; birdcage archived+GPL, gaol/extrasafe dead [sandbox] |
| Session storage | rusqlite or sqlx-sqlite, **size-capped rotating logs** | 0.9.x | codex pattern; explicitly avoid its unbounded-SQLite-log bug (640 TB/yr) [competitors] |
| Secrets | keyring (per-platform stores + encrypted-file fallback), in `spawn_blocking` | 4.1.4 | v4 facade; headless Linux needs the file fallback [rust-stack] |
| Config | figment (defaults < file < env < flags) | 0.10.19 | Profile layering fits sandbox/approval modes; dormant-but-stable. Runner-up: plain toml+serde (codex style) — genuinely close [rust-stack] |
| CLI | clap + clap_complete | 4.6.1 | Uncontested [rust-stack] |
| PTY / exec | portable-pty | 0.9.0 | codex-validated [rust-stack] |
| Release | cargo-dist + cargo-binstall artifacts | 0.32.0 / 1.20.1 | Shell/PowerShell/Homebrew installers are table stakes; watch cadence (axo shutdown) [rust-stack] |
| Testing | insta (TUI frame snapshots) + color-eyre | 1.48 / 0.6.5 | codex-validated [rust-stack] |
| License | MIT | — | Short, permissive, familiar, and compatible with broad open-source adoption. |

**Flagged cross-report conflict:** [competitors] recommends AppContainer for Windows sandboxing; [sandbox] documents that OpenAI explicitly evaluated and **rejected** AppContainer (capability model wrong shape) in favor of restricted tokens. Resolved: restricted tokens (see §3).

---

## 2. Grok API Integration Essentials

All from **[xai-api]** unless noted.

**Endpoint & shape**
- Primary: `POST https://api.x.ai/v1/responses` (OpenAI Responses-API shape, stateful-optional). **Do not build on `/v1/chat/completions`** — documented legacy/deprecated; server-side tools, encrypted reasoning, and chaining are Responses-centric.
- Base URL must be **user-configurable data, not a constant**: xAI→SpaceXAI reorg means api.x.ai migrates within ~12 months.
- Auth: `Authorization: Bearer $XAI_API_KEY`. Validate model IDs against `GET /v1/models` at startup (retired slugs silently redirect and re-price).

**Model routing defaults**
| Role | Model | Why |
|---|---|---|
| Flagship / plan mode | `grok-4.5` | 500k ctx, $2/$6, best coding; reasoning always-on (default high) — not for latency-sensitive paths |
| Agentic inner loop (default) | `grok-build-0.1` | 256k ctx, $1/$2, always-on reasoning, purpose-built coding model |
| Whole-repo / 1M ctx / quick edits | `grok-4.3` | 1M ctx, $1.25/$2.50, supports reasoning effort "none" (unverified as explicit API value — test) |
| Deep-research (tier-gated) | `grok-4.20-multi-agent-0309` | Parallel multi-agent, `xhigh` effort; only 9 RPS at Tier 0 — throttle |

**Client design constraints**
- **Streaming:** SSE with typed events, incl. `reasoning_summary_text.delta` (drive a collapsible "thinking" pane). **Tool calls arrive whole in a single chunk** — no partial-JSON tool-call parsing needed for xAI, but the multi-provider abstraction must still support incremental tool-call deltas for OpenAI/Anthropic backends.
- **Function calling:** ≤200 tools/request, `parallel_tool_calls` on by default, JSON Schema params.
- **Server-side tools mixable with client tools** in one request: `web_search`, `x_search` (Grok-exclusive live X data), code execution (type string inconsistent in docs — probe live), remote MCP (`{type:'mcp', server_url}`). Separately metered ($5–10/1k calls) — meter and display in the TUI.
- **Prompt caching:** automatic prefix caching; keep stable message prefixes, send `x-grok-conv-id` header, surface `usage.input_tokens_details.cached_tokens` in the cost display (cached input is 4–6x cheaper).
- **Structured outputs:** `response_format: json_schema` with guaranteed conformance — use for plan generation, edit manifests, commit messages.
- **Reasoning effort** (`low/medium/high`, `xhigh` on multi-agent) is the primary cost/latency knob: plan=high, edit=low.
- **Vision caveat:** don't use server-side stored history when images are in context (documented failure mode) — prefer stateless requests.
- Ship the xAI Docs MCP server (`https://docs.x.ai/api/mcp`) as an optional built-in connection.
- Multi-model fallback: any OpenAI/Anthropic-compatible endpoint + Ollama — the hedge against xAI API churn (Live Search 410 already broke superagent grok-cli) [competitors].

---

## 3. Sandbox Strategy

Architecture (all phases): one platform-agnostic `SandboxPolicy` struct (writable/readable roots, unreadable globs, protected metadata, network mode `Isolated|ProxyRouted|Full`) compiled by per-platform backends in a `grokforge-sandboxing` crate, plus a **denial classifier** so the TUI distinguishes "sandbox blocked this" from real failure and offers one-keystroke escalation. Approval matrix: 4 policies (untrusted/on-request/on-failure/never) × 3 modes (read-only/workspace-write/danger-full-access). Add `grokforge debug sandbox -- <cmd>`. **[sandbox]**

**MVP (ship with v1)**
- **Linux:** in-process, zero external deps — landlock 0.4.5 at ABI::V5 BestEffort (read-only `/`, full access on writable roots + /dev/null) **+** seccompiler network-deny filter (EPERM on connect/bind/listen/sendto etc. except AF_UNIX; hard-block io_uring_setup/enter/register, ptrace, process_vm_*) **+** `PR_SET_NO_NEW_PRIVS`. Surface `RulesetStatus` in the TUI — never silently degrade. Kernel floor 5.13.
- **macOS:** dynamically generated SBPL exec'd via hardcoded `/usr/bin/sandbox-exec -p <policy> -D PARAM=path` — paths always as `-D` parameters (never string-interpolated; injection risk). Author the policy locally from documented platform behavior. Startup self-test (`sandbox-exec -n no-network true`) with graceful approval-only fallback.
- **Windows:** delegate to WSL2 (run the Linux backend inside), native fallback = sandbox-off + explicit per-command approval. Matches Claude Code/nono; already beats nothing.
- Default-deny egress in workspace-write mode from day one.

**Phase 2**
- **Linux:** bundled SHA256-pinned bwrap ≥0.11.2 (userns only, never setuid — CVE-2026-41163) for pid/net namespaces, tmpfs masking, ro-bind carveouts; detect Ubuntu 24.04 `apparmor_restrict_unprivileged_userns=1` and print the fix; keep Landlock+seccomp as automatic fallback.
- **Network:** L7 domain-allowlist proxy (HTTP CONNECT + SOCKS5) living **outside** the sandbox, reached via unix socket (Linux) / allowed loopback port (macOS), with interactive "allow domain?" TUI prompts — the most user-visible sandbox feature.
- **Windows native (unelevated):** CreateRestrictedToken (write-restricted SIDs) + CreateProcessAsUserW + job object + private desktop; document that deny-read is unenforceable in this mode.
- Container fallback: `GROKFORGE_SANDBOX=docker|podman` + published devcontainer with iptables/ipset egress allowlist.

**Phase 3 (post-PMF differentiator)**
- codex-style elevated Windows dual-user model (offline user + firewall deny-all-outbound, one-time UAC setup binary, elevated command-runner over framed IPC). Budget for AV/GPO failure modes (errors 1385/5/87).

**Git-native tension — decided:** perform all git mutations from the **trusted host process** (agent requests commit → host executes); inside the sandbox, `.git` stays deny-write. This resolves the conflict between git-native workflow and the codex/Claude `.git`-protection convention, and closes the hooks/config injection vector. **[sandbox]**

**Do not depend on:** birdcage (archived, GPL-3.0), gaol (dead 2019), extrasafe (stale, x86_64-only). **[sandbox]**

---

## 4. Competitive Positioning — Top 5 Gaps → Feature Decisions

From **[competitors]**, with **[xai-api]** and **[sandbox]** support.

1. **Grok Build's privacy scandal (full-repo + .env uploads to xAI GCS buckets, HN 48877371).** → Ship a **"context ledger"**: a TUI panel showing exactly which files/bytes leave the machine per request, with .env/secret redaction on by default and zero telemetry without opt-in. MIT-licensed, local-first, BYO API key (pay-as-you-go vs Grok Build's SuperGrok gate). This is the marketing wedge; the implementation must be airtight or HN will find the gap.

2. **No open agent has Codex-class cross-platform sandboxing.** → The §3 plan **is** the feature. Headline it: read-only/workspace-write/full-access modes, default-deny network, per-command escalation. Don't overpromise Windows native before Phase 2 lands — that burns trust.

3. **Aider is abandoned (no feature release since Aug 2025) and its users are shopping.** → Git-native core: auto-commit every agent edit with descriptive messages, `/undo` = git revert, diff-first review UX, tree-sitter repo map for cheap context, **plus** worktree-per-subagent parallelism (Grok Build validated it; matches xAI's own table stakes) and PR-ready branch mode.

4. **Resource bloat (opencode 1GB+ RAM, Codex's 640 TB/yr log bug).** → Hard budget: <100 MB RAM, instant startup, size-capped rotating logs; publish the benchmark comparison. Rust + the §1 stack makes this nearly free.

5. **The Grok-specific niche is weakly held (superagent grok-cli: 3.2k stars, Bun, macOS-only sandbox, open critical security findings) and Grok Build is closed/Grok-only.** → **Grok-first, not Grok-locked**: native `x_search`/`web_search` server-side tools (no other agent ecosystem has live X data), reasoning-effort routing, prompt-cache stats — plus any OpenAI/Anthropic-compatible endpoint and Ollama as fallback. Match Grok Build's table stakes: Plan Mode, ≤8 subagents in worktrees, MCP, **ACP** (editor embedding — most open agents lack it), headless `-p` JSON mode, AGENTS.md/Agent Skills conventions.

Also adopt: release discipline (semver, real changelogs — opencode's top complaint), and check "Grok" trademark exposure for the GrokForge name early.

---

## 5. TUI Architecture Decisions

All from **[tui-patterns]** (codex-rs is the canonical reference), versions per **[rust-stack]**.

**Scrollback — decided: hybrid inline viewport (codex pattern), not alt-screen.**
- `Viewport::Inline` by default; finalized history cells committed into the terminal's **native** scrollback via ratatui's `scrolling-regions` feature (flicker-free `insert_before`). Preserves native scroll/select/copy — the #1 complaint against alt-screen agents (crush, Claude Code #71438).
- `alternate_screen = auto|always|never`; `auto` disables alt screen under `ZELLIJ`; implement a ZellijRaw-style raw-write path. Alt screen only for temporary overlays: transcript view (searchable, app-scrolled) and fullscreen diff/approval.
- Wrap every draw in synchronized output (DEC mode 2026); never full-screen Clear per frame.
- Runner-up (rejected): crush-style full alt-screen with app-level selection — more layout control, but forfeits native terminal ergonomics.

**Streaming render — decided: stable-queue + mutable live tail.**
- Buffer SSE deltas until newline-terminated; render committed source with pulldown-cmark; partition into a **stable region** (drained to scrollback via adaptive commit ticks keyed on queue depth + staleness) and a **mutable live tail** in the active cell.
- **Hold back tables and unclosed fenced code** from the stable queue — later tokens reshape layout and scrollback is immutable. Keep URL-only lines unwrapped (hyperlink detection).
- Never re-parse/re-wrap the transcript per token (the canonical O(n²) AI-TUI bug — goose #7223, crush #1746): pre-wrap each cell once per (content, width, theme), cache, re-wrap only on resize, virtualize overlays, cache syntax highlighting by (language, code), throttle live-cell re-render. Reasoning deltas (`reasoning_summary_text.delta` [xai-api]) render into a collapsible thinking cell on the same pipeline.

**Input handling — decided: custom composer widget.**
- Grapheme-cluster cursor movement (unicode-segmentation), unicode-width measurement, per-width wrapped-line cache, kill ring, input history, optional vim modal editing, atomic TextElement ranges for @-mentions/placeholders.
- Bracketed paste + burst-paste heuristic fallback; nucleo+ignore-backed @-file popup; filtered slash-command popup.
- Event loop: crossterm `EventStream` in `tokio::select!` with app channels.
- Approvals: bottom-pane modal (not a separate screen) with ordered options (proceed / don't-ask-again-this-session / deny / deny-with-feedback), single-key shortcuts, syntax-highlighted `$ command` preview or inline diff, fullscreen escape hatch.
- Diffs: syntect state preserved per hunk, line-number gutter + sign column, 3-tier color ladder (truecolor/ANSI-256/ANSI-16 — reuse codex's palette), DIM deleted lines, ~10k-line highlight cap.
- Light/dark via OSC 10/11 **in raw mode with timeout** + COLORFGBG/WT_SESSION/registry fallbacks and a config override (tmux leaks replies otherwise). Ship .tmTheme theming with a `/theme` live-preview picker. Test matrix must include tmux, Zellij, screen, SSH, Windows Terminal.

---

## 6. Top Risks & Mitigations

| # | Risk | Mitigation |
|---|---|---|
| 1 | **xAI platform churn** — SpaceXAI reorg will move the api.x.ai domain; retired slugs silently redirect and re-bill; features may land gRPC-first [xai-api] | Data-driven provider config (base URL, model IDs); validate against `GET /v1/models` at startup; provider-abstraction trait + integration tests; multi-model fallback as structural hedge |
| 2 | **xAI squeezes independent clients** — Grok Build is xAI's own funded Rust/ratatui agent; API degradations already hit grok-cli (prompt-cache regression, Live Search 410) [competitors] | Position on what xAI won't do: open source, BYO-key, multi-model, verifiable privacy. ACP + OpenAI/Anthropic/Ollama support means GrokForge survives Grok-hostile moves |
| 3 | **Model quality ceiling** — grok-build-0.1 at ~70.8% SWE-Bench (xAI-claimed) trails Claude/GPT frontier [competitors] | Multi-model fallback is the hedge, not a fix; route plan-mode to grok-4.5 high effort; make model switching mid-session first-class |
| 4 | **macOS sandbox-exec deprecation** — no published replacement; SBPL is private API and can break per-OS release [sandbox] | Startup self-test with graceful approval-only fallback; every major agent shares this risk (industry-wide, not GrokForge-specific) |
| 5 | **Linux userns restrictions** (Ubuntu 24.04 AppArmor breaks bwrap; Debian following) [sandbox] | Keep the namespace-free Landlock+seccomp path as automatic fallback; detect and print the AppArmor fix instead of failing cryptically |
| 6 | **Windows native sandboxing is genuinely hard** (deny-read unenforceable unelevated; AV/GPO breakage) [sandbox][competitors] | Phase it (WSL2 → unelevated tokens → elevated dual-user); never market "cross-platform sandboxing" ahead of what's shipped |
| 7 | **Ecosystem version cliffs** — ratatui 0.30 stranded widgets; reqwest 0.13 orphaned middleware; rmcp 2.x breaking + MCP spec drift; fastembed's exact ort rc pin [rust-stack] | Verify every widget crate declares ratatui ^0.30/ratatui-core ^0.1; isolate rmcp behind an internal trait; own the composer and markdown renderer (fewest third-party TUI deps); defer fastembed to post-MVP |
| 8 | **Silent security degradation** — Landlock BestEffort no-ops on old/disabled kernels; Landlock alone can't block DNS/UDP exfil pre-ABI-10; io_uring bypasses socket-syscall deny lists [sandbox] | Always pair Landlock with the seccomp net filter; block io_uring/ptrace explicitly; surface `RulesetStatus` in the TUI — never claim enforcement that isn't active |
| 9 | **TUI perf/compat landmines** — O(n²) re-render, unicode-width emoji drift, OSC reply leakage, multiplexer breakage [tui-patterns] | The §5 decisions are the mitigations; add insta snapshot tests and a tmux/Zellij/SSH CI matrix from day one |
| 10 | **Reputational blast radius on privacy claims** — any hidden telemetry or remote calls (opencode's title-generation scandal), unsigned binaries [competitors] | Context ledger + opt-in-only telemetry enforced in code review; sign/notarize macOS binaries; server-side tool calls ($5–10/1k) metered and displayed so users never get surprise bills [xai-api] |
