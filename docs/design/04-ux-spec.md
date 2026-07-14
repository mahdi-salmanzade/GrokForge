# GrokForge v0.1 — UX Specification: TUI, Git-Native Workflow, Safety Surfaces

Scope: full differentiator build (sandboxing, git-native, repo map/local RAG, MCP, skills, plan mode). Grok-only client against `api.x.ai/v1/responses` (base URL + model IDs are config). Binary: `grokforge`. Everything below is v0.1 unless tagged **[post-v0.1]**.

One resolved tension: user decision 1 puts "repo map/local RAG" in v0.1; the brief defers the *embedding* index (fastembed/usearch) post-MVP. Resolution: v0.1 local RAG = tree-sitter repo map + lexical retrieval (nucleo/ignore + ripgrep-style search tools); the embedding backend is a **[post-v0.1]** upgrade behind the same UX surfaces (`/map`, auto-context, ledger lines), so no UX change later.

---

## 1. TUI Layout

### 1.1 Main chat view (inline viewport, native scrollback)

Per the brief: `Viewport::Inline` by default; finalized cells committed into native terminal scrollback via ratatui 0.30 `scrolling-regions` (`insert_before`); every draw wrapped in DEC 2026 synchronized output; alt-screen used only for overlays (transcript, fullscreen diff, ledger, pickers); `alternate_screen = auto|always|never` with `auto` disabling under `ZELLIJ`.

Vertical composition (bottom-anchored live region, everything above is native scrollback):

```
  (native terminal scrollback — scroll/select/copy with your terminal)

  › you                                                            14:32
    Fix the flaky retry test in net/backoff.rs

  ● grok-build-0.1                                      [thinking ▸ 2.1s]
    The flake comes from an unseeded RNG in `jitter()`. I'll pin the
    seed in tests and widen the assertion window.

    ✎ edit net/backoff.rs  (+9 −4)                                 view: d
    $ cargo test backoff          ✓ 14 passed              (sandboxed ●)
    ⎿ committed a1f3c92  fix(net): seed jitter RNG in tests

  ● working — cargo clippy…  ⠸                            Esc to interrupt
┌────────────────────────────────────────────────────────────────────────┐
│ › Also add a property test for max delay_                              │
└────────────────────────────────────────────────────────────────────────┘
  grok-build-0.1 · effort low · ●auto ws-write · ⎇ gf/fix-backoff
  ctx 38% (97k/256k) · $0.41 (61% cached) · ↑3f/9.2KB · srv:0 · ? help
```

Cell taxonomy (each pre-wrapped once per (content, width, theme), cached; only the live tail re-renders):
- **User cell** `› you` — includes @-mention pills and collapsed paste pills (`[pasted 213 lines]`).
- **Assistant cell** — streaming markdown via the stable-queue + mutable-live-tail pipeline; tables/unclosed fences held back from scrollback commit.
- **Thinking cell** — collapsible; fed by `reasoning_summary_text.delta`; header shows elapsed time; `t` on live cell / config `thinking = collapsed|expanded|hidden`.
- **Tool cells** — one-line summaries with expanders: `✎ edit <file> (+a −b)`, `$ command → status (sandboxed ●/unsandboxed ⚠)`, `⊕ read <file> (n KB, k secrets redacted)`, `⚙ mcp:<server>.<tool>`, `☁ web_search "query" ($0.01)`.
- **Git cells** `⎿ committed <sha> <message>` — every host-side git mutation gets a visible cell (auditability).
- **System/banner cells** — sandbox degradation, ledger threshold prompts, session events.

### 1.2 Composer

Custom widget (grapheme-aware, wrap cache, kill ring, input history, atomic TextElements). Single-line border box that grows to max ~40% of viewport, then inner-scrolls. Placeholder: `Ask Grok · @ files · / commands · ? shortcuts`. Right-aligned draft size indicator appears when draft > 1 KB (`~2.1k tok`). Bracketed paste + burst heuristic → paste pill. While a turn is running the composer stays live (queue next message; shown as `queued ▸` chip).

### 1.3 Status line (persistent, single line, priority-truncating from the right)

Order (drop from right when narrow): `model` · `effort` · `sandbox glyph+preset+mode` · `⎇ branch (+dirty *)` · `ctx %` · `session cost (+cache hit %)` · `ledger compact ↑files/bytes` · `srv-tool spend` · `hint`.
- Sandbox glyph: `●` fully enforced, `◐` degraded (RulesetStatus partial), `○` off. `YOLO` renders red, blinking-free but impossible to miss.
- Plan mode replaces preset segment with `▤ PLAN`.
- Vim mode indicator `[N]/[I]` leftmost when enabled.
- Transient second hint line during activity: `Esc interrupt · Ctrl+T transcript · Ctrl+O ledger`.

### 1.4 Transcript overlay — `Ctrl+T` (alt-screen)

Full session transcript, app-scrolled, virtualized. Un-truncated tool output and full reasoning cells. Keys: `↑↓/j k` scroll, `g/G` top/bottom, `/` search (nucleo over cell text), `n/N` next/prev match, `Space` expand/collapse cell, `y` copy focused cell to clipboard (OSC 52), `q`/`Esc`/`Ctrl+T` close.

### 1.5 Approval overlay

Bottom-pane modal replacing the composer (never a separate screen), per the brief. Single-key: `y` approve once, `a` approve for session (scoped: this command prefix / this file / this domain), `d` deny, `e` deny + one-line feedback to the model, `f` fullscreen diff/output, `Esc` = deny (safe default), `Enter` = highlighted default (`y` except in `strict`, where default is `d`). Wireframe with inline diff:

```
┌ approval ── edit src/auth/session.rs ──────────────────────────────────┐
│  why: rotate the session token on privilege change                     │
│                                                                        │
│  src/auth/session.rs                                        1 of 1 hunk│
│  142 142 │      fn elevate(&mut self, role: Role) {                    │
│  143     │ -        self.role = role;                                  │
│      143 │ +        self.role = role;                                  │
│      144 │ +        self.token = SessionToken::rotate(&self.token);    │
│      145 │ +        self.touched = Instant::now();                     │
│  144 146 │      }                                          (+3 −1)     │
│                                                                        │
│  ▸[y] approve   [a] approve edits to this file for session   [d] deny  │
│   [e] deny + explain   [f] fullscreen   Esc deny                       │
└────────────────────────────────────────────────────────────────────────┘
```

Command variant shows syntax-highlighted `$ command`, cwd, and the sandbox verdict line (see §4). Approvals from subagents enter the same queue tagged `[agent 2/format]`.

### 1.6 Diff view

- Inline (in approval modal and `✎` cell expansion): capped ~20 lines, `f` to go fullscreen.
- Fullscreen (alt-screen): syntect per-hunk state, line-number gutter + sign column, DIM deleted lines, 3-tier color ladder, ~10k-line highlight cap, per-file tabs. Keys: `j/k` scroll, `[`/`]` prev/next hunk, `Tab/Shift+Tab` next/prev file, `q/Esc` back (returns to pending approval if one is active).
- `/diff` opens fullscreen diff of **cumulative agent changes this session** (session base ref → HEAD + worktree), not just last edit.

### 1.7 Session picker — `/sessions`

Alt-screen list backed by SQLite session store (size-capped, rotating). Columns: relative time, project/branch, first-prompt summary, turns, cost. Nucleo filter-as-you-type. `Enter` resume, `d` delete (confirm), `Esc` cancel. `/resume` = resume most recent for this project without the picker; `/resume <id>` direct.

### 1.8 `/theme` picker

Alt-screen list of bundled .tmTheme themes + `auto (terminal)`. Live preview: selection re-renders a sample pane (markdown + code + diff) instantly; `Enter` applies + persists to config, `Esc` reverts. Header shows detection result: `terminal bg: dark (OSC 11) · override: none`.

### 1.9 Context-ledger panel — `Ctrl+O` / `/ledger` (the privacy differentiator)

Alt-screen overlay; per-request audit of exactly what left the machine. Data captured at the single serialization choke point in the xai client crate — the only first-party code path with network egress. Post-redaction bytes are what's recorded.

```
┌ context ledger ── session 8c2f · endpoint api.x.ai · 14 requests ──────┐
│ outbound total 412 KB · secrets redacted 5 · files blocked 2 · tel OFF │
│────────────────────────────────────────────────────────────────────────│
│ ▸ #12 14:31  grok-build-0.1        41.2 KB   3 files   1 redaction     │
│ ▾ #13 14:32  grok-build-0.1        22.7 KB   2 files   0 redactions    │
│      conversation (system + history)              9.1 KB               │
│      repo map (tree-sitter, 214 symbols)          4.6 KB               │
│      @ net/backoff.rs                             6.2 KB   mention     │
│      ⊕ net/retry.rs                               2.8 KB   tool read   │
│      ✗ .env                                     BLOCKED  secrets.deny  │
│ ▸ #14 14:33  grok-4.3 · commit-msg                 0.9 KB   0 files    │
│────────────────────────────────────────────────────────────────────────│
│ server-side tools: web_search ×1 ($0.01) · x_search ×0                 │
│ mcp servers: 2 local processes — external egress not audited (see /mcp)│
│ ↑↓ select · Enter expand · s save audit JSON · q close                 │
└────────────────────────────────────────────────────────────────────────┘
```

Semantics:
- Every request row: model, endpoint host, total bytes, per-source breakdown (conversation, repo map, each file with inclusion reason: `mention | tool read | auto-context`), cached-prefix note (`61% served from prompt cache`).
- **Secret redaction indicators:** built-in rules on by default (`.env` values, AWS/GCP/generic API key patterns, `-----BEGIN … PRIVATE KEY-----` blocks). Redacted spans replaced with `[REDACTED:<rule>]` before serialization; count shown per request and echoed inline on the originating tool cell (`⊕ read config.rs (2 secrets redacted)`).
- **Blocked files:** `secrets.deny` globs (default `.env*`, `*.pem`, `id_rsa*`, `*.key`, `*.p12`) are never sent — content replaced by a placeholder telling the model the file is blocked; overridable only via an explicit approval prompt.
- **Pre-flight guardrail:** `ledger.confirm_over_kb` (default 1024) — a request exceeding it pauses with `About to send 1.4 MB (34 files) — [y] send · [v] view ledger · [d] cancel`.
- `s` writes `ledger-<session>.json` (same schema as headless `--ledger`).
- Honesty note surfaced in-panel: MCP servers are external processes; their own egress is out of audit scope and they are flagged as such in `/mcp`.

---

## 2. Slash Commands (v0.1 set)

| Command | Semantics (one line) |
|---|---|
| `/help` | Command list + keybindings + docs link; `? ` on empty composer does the same. |
| `/status` | One card: model/effort, sandbox backend + RulesetStatus, approval preset, git branch/base, session id, ctx/cost/cache stats, ledger totals, version. |
| `/model [id]` | Picker over cached `GET /v1/models` (ctx window, price, reasoning) with per-role defaults; mid-session switching is first-class; includes effort column. |
| `/effort <low\|medium\|high>` | Quick setter for reasoning effort (primary cost/latency knob); shortcut into `/model`'s second column. |
| `/theme [name]` | Live-preview theme picker (§1.8). |
| `/new` | Start a fresh conversation in the same project (current session saved). |
| `/compact` | Summarize history via structured outputs into a compact context cell; shows before/after tokens; warns it resets the prompt-cache prefix. |
| `/plan` | Toggle plan mode: read-only preset + plan model (default `grok-4.5`, effort high); ends with `[e]xecute / [r]evise / [s]ave plan` gate. |
| `/agents` | Subagent panel: id, task, worktree branch, status, cost; `Enter` live tail, `m` merge, `x` discard, `a` jump to pending approval. |
| `/undo [n]` | Undo last n agent commits (this session only) — reset if tip+unpushed, else revert; never touches user commits (§5.3). |
| `/diff` | Fullscreen cumulative diff of agent changes this session (§1.6). |
| `/commit [msg]` | Squash agent micro-commits since last user commit into one user-authored commit; message generated (editable) unless given; runs hooks. |
| `/branch [name]` | Create/switch session work branch `gf/<slug>` (slug generated from task if omitted). |
| `/pr` | Push branch (approval — network+remote), generate PR title/body via structured outputs, run `gh pr create` if present else print the command. |
| `/sessions` | Session picker overlay (§1.7). |
| `/resume [id]` | Resume most recent (or given) session for this project. |
| `/ledger` | Open context ledger (§1.9); `= Ctrl+O`. |
| `/map` | Show the tree-sitter repo map being sent (symbols, token cost); `refresh`, `on/off` subcommands. |
| `/sandbox [preset]` | Show sandbox backend, mode, RulesetStatus, writable roots; switch preset (`readonly/auto/strict`); `grokforge debug sandbox -- <cmd>` is the CLI sibling. |
| `/yolo` | Session-only toggle to `danger-full-access + never`; type-`yolo`-to-confirm; never persisted (§4.5). |
| `/mcp` | List MCP servers (stdio/HTTP), health, tool counts, "egress not audited" flag; enable/disable; includes optional built-in xAI Docs server (`https://docs.x.ai/api/mcp`). |
| `/skills` | List discovered skills/AGENTS.md-adjacent conventions (`.grokforge/skills/`, project + user scope); inspect one. |
| `/init` | Scan repo (repo map, manifests, README) → generate `AGENTS.md` via structured outputs → present as diff → approve to write. |
| `/login` | Create or unlock the password-encrypted credential file, then replace the API key or subscription OAuth tokens. |
| `/quit` (`/exit`) | Exit; prints session id + resume hint. |

Cut for v0.1: `/web`, `/x` (server-side tools are model-invoked, config-toggled, ledger-metered — a `/tools` toggle command is **[post-v0.1]**), `/voice`, `/review` (a subagent role, not a command) **[post-v0.1]**, `/copy` (transcript `y` covers it), `/redo` **[post-v0.1]**.

---

## 3. Input: @-mentions, Autocomplete, Keybindings, Vim

### 3.1 @-mention flow
`@` opens a popup anchored above the composer: nucleo fuzzy matcher over an `ignore`-walked file list (respects `.gitignore` + `secrets.deny` files shown 🔒-flagged and non-attachable without override), ranked by fzf-style score + recency (recently read/edited by agent boosts). Type to filter; `↑↓`/`Ctrl+P/N` navigate; `Tab`/`Enter` inserts an atomic pill `@src/main.rs` (backspace deletes whole pill); `Esc` closes leaving literal `@`. Directories attach a listing; image files attach as vision input (request goes stateless per the vision caveat). Content is read at send time; byte counts land in the ledger. Symbol-level mentions (`@fn:parse_args`) **[post-v0.1]**.

### 3.2 Slash autocomplete
`/` at column 0 opens the filtered command popup (name + one-line description + arg hint); substring/fuzzy filter; `Tab` completes name, second `Tab` cycles arg suggestions (e.g., `/model` model ids, `/theme` theme names); `Enter` runs when unambiguous.

### 3.3 Keybinding baseline

| Key | Action |
|---|---|
| `Enter` | Send (or accept popup selection) |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+J` | Newline (Shift+Enter needs kitty-protocol terminals; others are the fallback) |
| `Esc` | **Interrupt** running turn; close popup/overlay; in approval = deny |
| `Esc Esc` (empty composer, idle) | Backtrack: fork from a previous user message (picker over your messages) |
| `Ctrl+T` | Transcript overlay |
| `Ctrl+O` | Context ledger (`O` = outbound) |
| `Ctrl+C` | Clear composer; twice within 1s = quit |
| `Ctrl+D` (empty) | Quit |
| `Ctrl+Z` | Suspend (SIGTSTP, terminal restored) |
| `Up/Down` (at buffer edge) | Input history |
| `Ctrl+R` | Nucleo search over input history |
| `Ctrl+A/E, Alt+B/F, Ctrl+W/K/U/Y` | Readline motions + kill ring |
| `Ctrl+X Ctrl+E` | Edit draft in `$EDITOR` |
| Approval: `y/a/d/e` | Once / session / deny / deny+explain (§1.5); `f` fullscreen; `r` retry-unsandboxed on denial prompts |
| `t` (on live thinking cell) | Collapse/expand reasoning |
| `?` (empty composer) | Keybinding cheat sheet |

PageUp/PageDown are left to the terminal in the main view (native scrollback is the point); they scroll inside overlays.

### 3.4 Vim mode note
`composer.vim = true` (config or first-run choice): modal editing **in the composer only** — `[N]/[I]` indicator in status line; v0.1 covers motions/operators/registers on the single draft buffer (`h j k l w b e 0 $ d c y p x u`, counts); `Esc` in insert → normal, `Esc` in normal = interrupt (matching global Esc). Ex commands (`:w`, `:%s`) and vim keys in overlays **[post-v0.1]**.

---

## 4. Approval & Sandbox UX

### 4.1 The matrix, surfaced as presets
Internally: 4 approval policies (`untrusted / on-request / on-failure / never`) × 3 sandbox modes (`read-only / workspace-write / danger-full-access`), all 12 reachable via config/flags. The UI surfaces four named presets:

| Preset | = policy × mode | Plain-language line (shown in `/sandbox`) |
|---|---|---|
| `readonly` | on-request × read-only | "Grok can read and search; anything that writes or mutates asks first. (/plan uses this.)" |
| `auto` **(default)** | on-request × workspace-write | "Edits and commands run sandboxed inside this workspace with network off; anything beyond asks first." |
| `strict` | untrusted × workspace-write | "Every command asks before running, even sandboxed ones (tiny known-safe list excepted). For repos you don't trust." |
| `yolo` | never × danger-full-access | "No sandbox, no questions. Session-only, loudly marked." |

`on-failure` is documented as the power-user policy (`approval_policy = "on-failure"`): run everything sandboxed, only surface the denial-classifier prompt when the sandbox blocks something.

### 4.2 Per-command escalation wording
When a tool call exceeds the sandbox, the approval modal states **what**, **why it needs escalation**, and **the exact boundary**:

```
┌ approval ── outside sandbox: network access ───────────────────────────┐
│  $ pip install -r requirements.txt                    cwd: ~/proj      │
│  sandbox: workspace-write denies network (default-deny egress)         │
│  model's reason: install deps before running tests                     │
│  ▸[y] run unsandboxed once   [a] allow network for this session        │
│   [d] deny   [e] deny + explain   Esc deny                             │
└────────────────────────────────────────────────────────────────────────┘
```

`a` scope is the *boundary*, not the command (session network / session write-to-`<path>` / session command-prefix `pip …`) and is always named in the option text. Boundary kinds: network, write outside workspace, read of unreadable glob, protected metadata (`.git`), unsandboxed exec.

### 4.3 Sandbox-denial classifier UX
Sandboxed command fails → classifier (exit status + stderr patterns + seccomp EPERM / Landlock markers) decides "sandbox-caused" vs "real failure":

```
✗ npm install — exit 1 · classified: blocked by sandbox (network deny)
  [r] retry without sandbox (asks approval)   [n] retry with network only
  [i] ignore — let Grok see the failure       [v] view stderr
```

Real failures are handed to the model without a prompt. Under `on-failure` policy this prompt *is* the approval surface. Misclassification escape hatch: `[i]` always available; `v` shows raw stderr with matched markers highlighted.

### 4.4 RulesetStatus / degradation warnings
- Startup + `/sandbox` + `/status`: per-backend enforcement table. Landlock `BestEffort` downgrade → yellow banner cell: `⚠ sandbox degraded: Landlock ABI v2 (kernel 5.15) — file isolation partial; seccomp network deny ACTIVE`. Never silent (brief risk #8).
- Fully unavailable (old kernel, macOS `sandbox-exec` self-test failure, Windows without WSL2) → red banner + automatic drop to `strict` (approval-only): `✗ sandbox unavailable — switched to strict: every command will ask. [details]`.
- Status-line glyph mirrors this: `●` full / `◐` degraded / `○` off — clicking is not a thing; `Ctrl`-free, `/sandbox` explains.
- Ubuntu 24.04 AppArmor userns detection prints the documented fix **[post-v0.1 — bwrap phase]**; v0.1's Landlock+seccomp path is namespace-free so v0.1 only ships the Landlock/seccomp/Seatbelt/WSL2 probes.

### 4.5 Session yolo toggle + guardrails
`/yolo` (or `--yolo` flag): requires typing `yolo` to confirm in the TUI; shows exactly what turns off (`sandbox OFF · approvals OFF · .git write-protection now advisory — git mutations still routed through GrokForge`). Guardrails: never written to config; red `YOLO` status segment; auto-expires at session end; refused when project policy `grokforge.toml` sets `forbid_yolo = true` (team control); `/yolo off` reverts to previous preset instantly; first destructive-looking command (`rm -rf`, `git push --force`, …) after enabling still gets one advisory confirm (`yolo_soft_confirm = true` default, settable off).

---

## 5. Git-Native Workflow

All mutations execute in the **trusted host process**; inside any sandbox, `.git/**` is deny-write (protected metadata in `SandboxPolicy`). Reads (`git status/log/diff/blame`) via gix in-process; mutations shell out to `git` CLI from the host.

### 5.1 When auto-commits happen
Foreground auto-commit is disabled in v0.1. Recording a path touched by a file tool is not enough
to prove ownership: a user or sibling process can race a write to that same path before staging.
Foreground edits therefore remain in the worktree for explicit review/commit.

Subagents are the safe exception. Each owns a mode-0700 worktree outside every parent project and
stages only descriptor-safe `write_file`/`edit` paths at turn end when every tool used in that
turn was an awaited built-in file/read operation. A shell, MCP, or custom tool disables the
automatic commit because a descendant can outlive Seatbelt process-group cleanup and race a
same-path write. Pre-command checkpoints remain a future design once content identity can be
bound through staging.

### 5.2 Commit message format (structured outputs)
Turn-end messages come from one cheap call (`git.commit_model`, default `grok-4.3`, effort low/none) with `response_format: json_schema`:
`{ type: "feat|fix|refactor|test|docs|chore", scope: string|null, summary: string (≤72, imperative), body: string|null }` → rendered `fix(net): seed jitter RNG in tests`. Every agent commit carries trailers (the machine-readable marker /undo keys off):

```
Grokforge-Session: 8c2f…
Grokforge-Turn: 14
```

Author and committer identity come from the user's Git configuration. Auto-commits run with hooks neutralized (`-c core.hooksPath=` + `--no-verify`) to close the hook-injection vector; `git.run_hooks = true` opts in; user-initiated `/commit` runs hooks by default. This call appears in the ledger like any other request (`#14 grok-4.3 · commit-msg · 0.9 KB`).

### 5.3 `/undo` semantics
- Walks back from HEAD over commits with the current session's `Grokforge-Session` trailer **only**; stops at the first foreign commit ("next commit is yours — stopping").
- Mechanism: if the target commit is at the tip, unpushed, and its paths are clean in the worktree → `git reset --keep <parent>` (tidy history); otherwise `git revert --no-edit` (safe, preserves history). The choice is displayed, with a diff preview + confirm.
- `/undo 3` repeats; a squash produced by `/commit` is user-authored, so /undo declines past it.
- Belt-and-braces: before any /undo mutation the host writes the pre-state to `refs/grokforge/backup/<session>/<n>` (dangling commit via `git stash create`-style plumbing), mentioned in the confirm ("recoverable via /status → backups").

### 5.4 Dirty worktree on start
Launch in a dirty repo → banner + choice (default `c`):

```
Worktree has uncommitted changes (7 files).
[c] continue — GrokForge only ever commits files it edits
[s] stash my changes first (git stash push)
[b] new branch from current state
[q] quit
```

If the agent later edits a file *you* have uncommitted changes in, that edit always prompts (regardless of policy) and the host snapshots the pre-edit blob to `refs/grokforge/backup/…` first — nothing of the user's is ever silently lost or committed.

### 5.5 Branch mode + PR-ready flow
- `git.auto_branch = ask|always|never` (default `ask`): first mutating turn while on the default branch prompts `You're on main — create gf/fix-login and work there? [y/n]`. `/branch` at any time; slug generated by the same structured-outputs call.
- `/pr`: `/commit`-squash offer → push (`git push -u origin gf/…` — approval: network + remote mutation) → PR title/body via json_schema → `gh pr create --title … --body …` if `gh` exists, else print the command. Body includes a session footnote (turns, model, cost — configurable off).

### 5.6 Worktree-per-subagent lifecycle
For each subagent (≤8, plan-mode fan-out or `/agents`):
1. **Create:** host runs `git worktree add <per-user-data>/worktrees/<agent-id> -b gf/agent/<id> <session-branch>` under an owner-private root outside all parent projects; the subagent's `SandboxPolicy` writable root = only that worktree. Its `.git` file/dir remains deny-write.
2. **Run:** subagent's edits/commits follow §5.1–5.2 with `Grokforge-Agent: <id>` trailer; its approvals funnel into the main queue tagged `[agent <id>]`; `/agents` shows live status/cost per worktree.
3. **Merge-or-discard:** completion produces a result card in `/agents`: `m` merge (host: squash-merge `gf/agent/<id>` into the session branch; conflicts open a special approval with the conflict diff and `[k]eep mine / [t]ake agent / [e]dit later` per file), `x` discard (`git worktree remove --force` + `git branch -D`).
4. **Cleanup:** worktrees pruned at session end; startup scans `.grokforge/worktrees` + `git worktree prune` for orphans (crash recovery) and offers resume-or-discard.

---

## 6. Headless Mode

`grokforge -p "fix the failing tests" [flags]` — no TUI, streams to stdout, exits.

Flags:
- `--json` — NDJSON event stream, one object/line: `session`, `turn.start`, `item.delta` (assistant text), `tool.call`, `tool.result`, `commit {sha,message}`, `approval.auto {action, boundary, resolution}`, `ledger.request {bytes, files, redactions}`, `turn.end {tokens, cost}`, `result`, `error`. Without `--json`: plain final text on stdout, progress on stderr.
- `--output-schema <file.json>` — final message forced through `response_format: json_schema`; the conforming JSON is stdout's last line / the `result` event.
- `--output-last-message <file>` — also write final text/JSON to a file.
- Approval/sandbox: `--preset <readonly|auto|strict|yolo>`, or granular `--sandbox <mode>` + `--approval <policy>`; headless semantics: any action that would prompt is **auto-denied** (model is told why; event `approval.auto`) unless `--yolo` or the boundary was pre-granted via `--allow <network|write:<path>|cmd:<prefix>>` (repeatable).
- `--model <id>`, `--effort <low|medium|high>`, `--cd <dir>`, `--max-turns N`, `--timeout <secs>`, `--no-repo-map`, `--no-auto-commit`, `--ledger <file>` (write the audit JSON — the CI-grade privacy artifact), `--resume <session-id>`, `-q/--quiet`.

Exit codes: `0` success · `1` task failed (agent reported) · `2` usage/config · `3` auth (key invalid / model slug missing from `/v1/models`) · `4` API/network · `5` blocked by policy (auto-denied escalation was fatal to the task) · `124` timeout · `130` interrupted.

CI example:

```yaml
- name: Grok fixes lint debt
  env: { XAI_API_KEY: ${{ secrets.XAI_API_KEY }} }
  run: |
    grokforge -p "fix all clippy warnings; keep behavior identical" \
      --preset auto --allow cmd:cargo --max-turns 12 \
      --json --ledger ledger.json --output-last-message summary.md
    git push origin HEAD:gf/clippy-fixes
```

`grokforge exec` is reserved as an alias **[post-v0.1]**; `grokforge debug sandbox -- <cmd>` and `grokforge resume` ship v0.1 as CLI subcommands.

---

## 7. First-Run Experience

Sequential wizard on first `grokforge` launch (each step skippable via flags/env; re-runnable via `grokforge setup`):

1. **Welcome** — one screen: name, version, "MIT", the privacy promise ("the only network calls go to the API endpoint you configure — verify anytime with Ctrl+O").
2. **Credentials** — a non-empty `XAI_API_KEY` environment value wins and skips file setup. Otherwise, set and confirm a GrokForge password, then choose subscription OAuth or a masked API-key prompt. Store the resulting credential in `~/.grokforge/credentials.enc`, sealed with ChaCha20-Poly1305 under an Argon2id-derived key; never write it to config files or an OS secret store. Later runs ask for the password to unlock the file.
3. **Model validation** — spinner on `GET /v1/models`; table of available models (id, ctx, $/M in/out, reasoning); default suggestion `grok-build-0.1` (agentic) + `grok-4.5` (plan); any *configured* slug missing from the list gets a loud warning (retired slugs silently redirect and re-price — brief §2). Base URL shown as editable config, not a constant.
4. **Sandbox self-test** — platform probes, results table:
   ```
   macOS Seatbelt   sandbox-exec -n no-network …  ✓ enforced
   network deny     probe connect 1.1.1.1:443     ✓ blocked
   .git protection  write probe                   ✓ blocked
   → default preset: auto (workspace-write, on-request)
   ```
   Degraded/failed rows in yellow/red with the consequence stated (`→ falling back to strict`), matching §4.4 exactly.
5. **Telemetry consent** — default **OFF**, opt-in question with a precise list of what opt-in would send; "No" is the pre-selected answer; decision recorded in config and echoed in `/status` and the ledger footer forever.
6. **Theme** — OSC 10/11 detection (raw-mode, timeout, COLORFGBG/WT_SESSION fallback) → "Detected dark background — theme: grokforge-dark. Change now? [/theme]".
7. **Project init** — if cwd is a git repo without `AGENTS.md`: offer `/init` → repo scan → generated `AGENTS.md` shown in the diff view → approve to write (this is also the first demonstration of the approval modal). Creates `.grokforge/` (sessions ref, worktrees dir) and offers a `.gitignore` entry.

---

## 8. Explicit post-v0.1 deferrals
Embedding-based semantic index (UX surfaces unchanged) · L7 domain-allowlist proxy with "allow domain?" prompts (v0.1 network approvals are all-or-nothing per session) · Windows native restricted-token sandbox (v0.1 = WSL2 delegation + approval-only fallback, per lock #3) · ACP editor embedding · `/tools` toggle command · symbol-level @-mentions · clipboard image paste · vim ex-commands/overlay vim keys · `/redo` · voice/Telegram.

---

### Critical Files for Implementation
(Greenfield — proposed paths; the brief is the settled input.)
- `crates/grokforge-tui/src/app.rs` — event loop and terminal UI.
- `crates/grokforge-core/src/turn.rs` — turn lifecycle, Git workflow, and subagents.
- `crates/grokforge-core/src/context.rs` — context ledger and redaction boundary.
- `crates/grokforge-sandbox/src/lib.rs` — sandbox selection and policy boundary.
