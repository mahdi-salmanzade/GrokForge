I have everything I need — the brief is comprehensive, the strategy doc adds nothing technical beyond what's locked, and I've verified the codex-rs workspace shape as the reference pattern. Below is the architecture design document.

---

# GrokForge v0.1 — System Architecture

Greenfield Rust workspace. Inputs: `/tmp/grokforge_brief.md` (settled stack decisions), user-locked decisions (full differentiator scope, Grok-only client, OS-native sandbox in v0.1, dual MIT OR Apache-2.0, name/binary `grokforge`).

**License consequence (applies throughout):** dual MIT OR Apache-2.0 overrides the brief's Apache-2.0-only pick. codex-rs (Apache-2.0) is a *pattern reference only* — this includes `seatbelt_base_policy.sbpl`, which the brief suggested copying; it must instead be **authored from scratch** (SBPL is code). Same for the seccomp syscall deny list: the *list of syscalls* (io_uring_setup/enter/register, ptrace, process_vm_*, socket family filtering) is uncopyrightable facts and fine to reproduce; the filter-building code is not.

**Provider consequence:** Grok-only, no provider trait in v1. The brief's "multi-model fallback" hedge is explicitly deferred; the mitigation retained is (a) `grokforge-xai` as an isolated crate (the future seam), (b) base URL + model IDs as config data, (c) startup `GET /v1/models` validation.

---

## 1. Cargo Workspace Layout

Sixteen crates under `crates/`, all named `grokforge-*` except the binary crate `grokforge` (the `cli`). One published binary. Workspace-level `[workspace.dependencies]` pins the brief's versions (ratatui 0.30.2, crossterm 0.29, tokio 1.52, reqwest 0.13.4, eventsource-stream 0.2.3, gix 0.85, rmcp 2.2, rusqlite 0.9 bundled, landlock 0.4.5, seccompiler 0.5, portable-pty 0.9, figment 0.10.19, clap 4.6, nucleo, tree-sitter 0.26, text-splitter 0.32, syntect 5.3 + two-face, similar/diffy, insta, keyring 4.1).

```
grokforge/
├── Cargo.toml                      # workspace
├── crates/
│   ├── protocol/      grokforge-protocol
│   ├── config/        grokforge-config
│   ├── xai/           grokforge-xai
│   ├── sandbox/       grokforge-sandbox
│   ├── exec/          grokforge-exec
│   ├── git/           grokforge-git
│   ├── mcp/           grokforge-mcp
│   ├── index/         grokforge-index
│   ├── file-search/   grokforge-file-search
│   ├── store/         grokforge-store
│   ├── core/          grokforge-core
│   ├── tools/         grokforge-tools
│   ├── render/        grokforge-render
│   ├── tui/           grokforge-tui
│   ├── headless/      grokforge-headless
│   └── cli/           grokforge          (bin: grokforge)
├── docs/
└── xtask/                           # cargo-dist config, grammar pinning checks
```

### Crate graph (arrows = depends-on; strict DAG)

```
                          ┌──────────────┐
                          │   protocol   │  (leaf: types only)
                          └──────▲───────┘
        ┌────────┬────────┬─────┼──────┬─────────┬──────────┐
        │        │        │     │      │         │          │
     config   sandbox    mcp  store  render   (core)     (tui/headless)
        ▲        ▲        ▲     ▲               │
        │        │        │     │               │
        │      exec ──────┘     │               │
        │        ▲              │               │
        │        │              │               │
   ┌────┴────────┴──────────────┴───┐
   │ core ── deps: protocol, config,│      xai ◄── core     (xai has NO internal deps)
   │  xai, store, mcp, git, index,  │      git ◄── core, tools
   │  sandbox (types/classifier)    │      index ◄── core
   └───────────────▲────────────────┘      file-search ◄── tools, tui
                   │
        ┌──────────┼───────────┐
      tools       tui       headless
        ▲          ▲            ▲
        └──────────┼────────────┘
                  cli  (also: sandbox for arg0 helper, exec for `debug sandbox`)
```

### Per-crate responsibility, modules, and boundary justification

**`grokforge-protocol`** — the shared vocabulary. Pure serde types, zero async, zero I/O, no tokio dep.
- Modules: `op` (Submission/Op), `event` (Event/EventMsg), `items` (InputItem, ResponseItem, ContentBlock), `approval` (ApprovalRequest/Decision), `sandbox_policy` (SandboxPolicy, SandboxMode, NetworkMode, DenialClass), `plan` (PlanState/PlanStep), `ledger` (LedgerEntry), `usage` (Usage, CostBreakdown), `ids` (SessionId, TurnId, ToolCallId, ApprovalId, SubId — newtyped UUIDs).
- *Boundary justification:* the codex "protocol" pattern — TUI, headless, and a future ACP/editor-embedding adapter all speak `Submission`/`Event` and never touch core internals. Fast-compiling leaf crate; frontends rebuild without recompiling the agent; protocol snapshot tests (insta on serde JSON) freeze the wire format.

**`grokforge-config`** — figment layering (defaults < `~/.grokforge/config.toml` < project `.grokforge/config.toml` < env `GROKFORGE_*` < flags). Owns: `Config`, `ProviderConfig { base_url: Url, api_key_env, model_ids: ModelRoutingTable }` (base URL/models are **data**, per lock #2 and brief §2), approval/sandbox profile matrix, theme, MCP server declarations, AGENTS.md/skills/plugins discovery paths (`~/.grokforge/skills/`, `.grokforge/skills/`), keyring access (`keyring` in `spawn_blocking`).
- *Justification:* config is consumed by literally every crate above the leaves; isolating it prevents figment/keyring from bleeding into `protocol`.

**`grokforge-xai`** — the in-house Grok client. **No internal deps** (takes a `ProviderConfig`-shaped struct it defines itself; `config` constructs it).
- Modules: `client` (XaiClient), `responses` (request/response wire types for `POST /v1/responses`), `stream` (SSE via `reqwest::bytes_stream` + `eventsource-stream`, own reconnect/backoff), `models` (`GET /v1/models` validation), `error` (typed: rate-limit w/ retry-after, auth, retired-slug detection).
- Implements brief §2: typed SSE events incl. `reasoning_summary_text.delta`; tool calls arrive **whole** (no partial-JSON accumulation needed); `x-grok-conv-id` header; `usage.input_tokens_details.cached_tokens` surfaced; `response_format: json_schema` support; `parallel_tool_calls`; server-side tools (`web_search`, `x_search`, code execution, remote MCP) as request mixins; reasoning-effort knob; stateless requests when images present (documented xAI failure mode).
- *Justification:* lock #2 — no provider trait now, but the crate boundary **is** the future seam. Also independently integration-testable against a mock SSE server (wiremock) with zero agent machinery.

**`grokforge-sandbox`** — policy compilation + platform backends + denial classifier.
- Modules: `policy` (compile `protocol::SandboxPolicy` → platform plan), `linux` (landlock ABI::V5 BestEffort + seccompiler net-deny filter + `PR_SET_NO_NEW_PRIVS`; exposes `apply_in_child()` for the pre-exec helper and `RulesetStatus` reporting), `macos` (SBPL generation — paths always via `-D PARAM=` never string-interpolated; `sandbox-exec` self-test `-n no-network true` at startup; graceful approval-only fallback), `windows` (WSL2 detection + delegation planner: rewrites commands to `wsl.exe -d <distro> -- grokforge __sandbox-helper …` with `/mnt/c` path translation; native fallback = report `Unsupported` → core forces approval-only), `classifier` (exit status + stderr + errno heuristics → `DenialClass { FsWrite, FsRead, Network, Signal }` so the TUI can distinguish "sandbox blocked this" from real failure and offer one-key escalation), `selftest` (startup capability probe → `SandboxCapability` report, surfaced in TUI — never silently degrade).
- *Justification:* the single most audited code in the project (positioning gap #2); must be unit-testable per-platform under `cfg` without dragging in exec/PTY; the CLI's arg0 helper entry links only this crate + std.

**`grokforge-exec`** — process lifecycle under sandbox. Deps: `sandbox`, `protocol`.
- Modules: `spawn` (build `Command` per sandbox plan; on Linux self-re-exec as `grokforge __sandbox-helper` which applies landlock+seccomp then `execvp`s the target — landlock must be applied in the child), `pty` (portable-pty sessions for interactive commands), `capture` (byte-capped ring capture, head+tail truncation, ~50ms chunk coalescing for the event channel), `timeout_kill` (process-group kill on cancel/timeout), `env` (env scrubbing: strip `XAI_API_KEY` etc. from child env).
- *Justification:* separate from `sandbox` because policy compilation and process management change for different reasons; separate from `tools` so `grokforge debug sandbox -- <cmd>` (brief §3) reuses it without the tool layer.

**`grokforge-git`** — deps: none internal (gix + std::process). The brief's split: **gix for reads** (status, diff, log, blame, worktree enumeration), **shell out to `git` CLI for mutations** — and per brief §3's decided git-native tension resolution, *all mutations run from the trusted host process*, never inside the sandbox; `.git` stays deny-write in `SandboxPolicy.protected_paths`.
- Modules: `read` (RepoSnapshot: status/diff/blame via gix), `mutate` (commit/branch/revert/stash via CLI, always with `-c core.hooksPath=/dev/null` style hook neutralization on agent-initiated mutations), `autocommit` (ghost-commit stack: every agent edit → commit with generated message; `/undo` = revert), `worktree` (create/remove/list under `.grokforge/worktrees/<task-id>`, merge-back: rebase or patch extraction), `identity` (attribute commits `Co-Authored-By: GrokForge`).
- *Justification:* Aider-replacement positioning (gap #3) makes git a first-class subsystem, not a tool detail; `tools` and `core` both consume it (edit tool auto-commits; subagent manager needs worktrees).

**`grokforge-mcp`** — rmcp 2.2 **behind an internal trait** (brief risk #7: rmcp breaking churn stays inside this crate). Deps: `protocol`.
- Modules: `connection` (`trait McpConnection { async fn list_tools(); async fn call_tool(name, args, cancel) -> McpResult; }`), `rmcp_backend` (stdio + streamable HTTP transports), `manager` (spawn/supervise servers from config, startup handshake timeout, restart-with-backoff), `schema` (MCP tool schema → `ToolSpec` translation), `builtin` (optional xAI Docs MCP `https://docs.x.ai/api/mcp`). Avoid deprecated sampling/roots/logging (SEP-2577).

**`grokforge-index`** — repo map + chunking; the Aider crown jewel. Deps: none internal.
- Modules: `tags` (tree-sitter tag extraction per language; grammars pinned on LanguageFn ABI, gated by an `xtask` check), `rank` (symbol-reference graph + personalized-PageRank ranking seeded by recently-mentioned/edited files), `map` (render ranked map to a **token-budgeted** text block; content-hash cache in `.grokforge/cache/repomap.bin`, incremental invalidation on mtime/hash), `chunk` (text-splitter `code` feature — used by the post-MVP semantic index), `semantic` (**feature-gated off in v0.1 default build**: fastembed + usearch — the ort rc pin stays out of the default dependency tree).
- *Justification:* CPU-heavy, model-agnostic, reusable by a future `grokforge index` CLI subcommand; keeps tree-sitter's C grammar builds out of core's incremental compile path.

**`grokforge-file-search`** — nucleo + ignore/globset fuzzy matcher (codex file-search pattern). Consumed by the TUI @-mention popup and the `glob` tool. Tiny, leaf.

**`grokforge-store`** — SQLite persistence + rotating logs. Deps: `protocol`.
- Modules: `db` (rusqlite, WAL, busy_timeout, migrations table), `sessions` (CRUD, resume/fork), `usage` (token/cost rollups), `ledger` (context-ledger rows), `logs` (**size-capped rotating** tracing appender — hard budget per brief gap #4; e.g. 5 MB × 5 files under `~/.grokforge/logs/`, enforced at write time, never unbounded).
- *Justification:* persistence format is a compatibility surface (resume across versions); isolating it lets `core` be tested against an in-memory store; explicitly walls off codex's 640 TB/yr unbounded-log bug class.

**`grokforge-core`** — the agent. Deps: `protocol, config, xai, store, mcp, git, index, sandbox`.
- Modules: `conversation` (ConversationManager: spawn sessions, hand out channel pairs), `session` (Session state, history), `turn` (TurnRunner state machine, §5), `tools` (the `Tool` **trait**, `ToolRegistry`, `ToolRouter` — trait lives here; implementations live in `grokforge-tools`, except the recursive `spawn_task` subagent tool which lives here to avoid a dependency cycle), `approvals` (ApprovalBroker: policy matrix 4 approval policies × 3 sandbox modes; pending-approval oneshot map), `context` (ContextAssembler: prompt construction, AGENTS.md chain, repo-map budget, @-mentions, redaction pipeline), `compact` (compaction, §5), `ledger` (ContextLedger recorder), `plan` (plan-mode state, §6), `subagent` (SubagentManager, §6), `cost` (usage aggregation, server-side tool metering).
- *Justification:* this is the ACP/editor-embedding reuse unit — everything a non-terminal frontend needs, with zero ratatui/crossterm in its dependency tree.

**`grokforge-tools`** — built-in tool implementations behind core's `Tool` trait. Deps: `core, exec, git, index, file-search, protocol`.
- Built-ins: `read_file`, `write_file`, `edit` (string-replace), `apply_patch` (unified diff via diffy; proposes `PatchProposed` event for diff-first approval), `shell` (via `exec`, sandboxed), `grep` (ripgrep-style via `ignore` + regex), `glob` (file-search), `git_status`/`git_diff`/`git_log` (read-only), `git_commit` (host-process mutation, approval-gated), `update_plan`, `web_fetch` (local, ledgered). Server-side Grok tools (`web_search`, `x_search`, code-exec, remote MCP) are **not** in this crate — they're request-level mixins handled in `core::context` + `xai`.
- *Justification:* inverting the dependency (tools → core, registry assembled in `cli` and injected) keeps the agent loop testable with mock tools and keeps exec/git/tree-sitter out of core's compile path.

**`grokforge-render`** — the streaming markdown/diff render pipeline (brief §5), deps `protocol` + `ratatui-core` (not full ratatui).
- Modules: `stream` (newline-gated commit queue: **stable region** vs **mutable live tail**; hold back tables and unclosed fences; URL-only lines unwrapped), `markdown` (pulldown-cmark 0.13 → styled lines), `highlight` (syntect fancy-regex + two-face; cache keyed `(language, code-hash)`), `diff` (similar-based hunks, per-hunk syntect state, gutter/sign column, 3-tier color ladder, DIM deletions, 10k-line cap), `wrap` (per-cell wrap cache keyed `(content-hash, width, theme)` — the O(n²) bug killer), `theme` (.tmTheme loading, OSC 10/11 detection with timeout + COLORFGBG/WT_SESSION fallbacks).
- *Justification:* the hardest-to-get-right TUI subsystem; pure-function core (bytes in → styled lines out) makes it insta-snapshot-testable without a terminal; depending only on `ratatui-core` future-proofs against widget-crate churn (risk #7).

**`grokforge-tui`** — ratatui frontend. Deps: `core` (spawn only), `protocol`, `render`, `config`, `file-search`.
- Modules: `app` (event loop: crossterm `EventStream` + core Event rx in `tokio::select!`), `chatwidget` (inline viewport, `insert_before` scrollback commits via `scrolling-regions`, DEC 2026 synchronized output, `alternate_screen = auto|always|never` with Zellij raw-write path), `history_cell` (HistoryCell enum, §2), `composer` (custom widget: grapheme cursor, wrap cache, kill ring, history, TextElement @-mention atoms, bracketed paste + burst heuristic), `popups` (@-file via file-search, slash commands, `/theme` live preview), `approval_modal` (bottom pane, ordered options, single-key, `$ command` syntax preview / inline diff, fullscreen escape hatch), `ledger_panel` (context ledger + cost/cached-token display), `overlays` (alt-screen transcript view, fullscreen diff), `status` (sandbox capability banner — `RulesetStatus` surfaced, never silent).

**`grokforge-headless`** — `grokforge exec -p "..."` runner. Deps: `core, protocol, config`. Maps EventMsg → JSONL on stdout (`--output json|text`), non-interactive approval resolution (`--full-auto` / default deny + nonzero exit), exit codes (0 done, 2 needs-approval-denied, 3 error). Same core, ~500 lines.
- *Justification:* the existence of two frontends over one channel pair is the proof the protocol boundary works — cheap CI harness for end-to-end agent tests.

**`grokforge` (cli)** — clap entry. Deps: `tui, headless, core, config, sandbox, exec`.
- Subcommands: default = interactive TUI; `exec/-p` headless; `resume`/`fork`/`sessions`; `debug sandbox -- <cmd>`; `mcp list/test`; `completions`; hidden `__sandbox-helper` (arg0-style dispatch: applies Linux landlock+seccomp in-process pre-exec — checked **before** clap parsing).

---

## 2. Core Domain Types & Trait Signatures

### 2.1 Protocol: the Op/Event channel pair (codex SQ/EQ pattern)

```rust
// grokforge-protocol
pub struct Submission { pub id: SubId, pub op: Op }

pub enum Op {
    UserTurn {
        items: Vec<InputItem>,              // Text | Image | FileMention{path,range} | SkillInvocation
        model_override: Option<String>,
        effort_override: Option<ReasoningEffort>, // low|medium|high|xhigh
        mode: TurnMode,                     // Execute | Plan
    },
    Interrupt,                              // Esc: cancel running turn + tools
    ApprovalDecision { request_id: ApprovalId, decision: Decision },
    ExecStdin { call_id: ToolCallId, bytes: Vec<u8> },   // interactive PTY input
    SetPolicy { approval: Option<ApprovalPolicy>, sandbox: Option<SandboxMode> },
    Compact,                                // manual /compact
    LedgerQuery,
    Shutdown,
}

pub struct Event { pub sub_id: SubId, pub msg: EventMsg }

pub enum EventMsg {
    SessionConfigured { session: SessionInfo, sandbox: SandboxCapability, models: Vec<ModelInfo> },
    TurnStarted { turn_id: TurnId, mode: TurnMode },
    AgentMessageDelta { delta: String },
    AgentMessageDone { text: String },
    ReasoningSummaryDelta { delta: String },              // -> collapsible thinking cell
    ToolCallBegin { call: ToolCallInfo },                 // name, args preview, sandbox mode
    ToolOutputDelta { call_id: ToolCallId, chunk: Vec<u8> },
    ToolCallEnd { call_id: ToolCallId, result: ToolResultSummary },  // exit, duration, denial: Option<DenialClass>
    PatchProposed { call_id: ToolCallId, diff: UnifiedDiff },        // diff-first review
    ApprovalRequested(ApprovalRequest),
    PlanUpdated(PlanState),
    LedgerAppended(LedgerEntry),
    TokenUsage { usage: Usage, cost: CostBreakdown },     // incl. cached_tokens, server-tool $ meter
    StreamRetrying { attempt: u32, reason: String },
    TurnComplete { turn_id: TurnId, stop: StopReason },   // EndTurn | Interrupted | MaxIterations | Error
    SubagentEvent { task_id: TaskId, boxed: Box<EventMsg> },  // subagent streams, tagged
    Error { message: String, recoverable: bool },
    ShutdownComplete,
}
```

### 2.2 Approvals

```rust
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub call_id: Option<ToolCallId>,
    pub kind: ApprovalKind,
    pub context: String,                    // why the agent wants this
}
pub enum ApprovalKind {
    ExecCommand { command: Vec<String>, cwd: PathBuf, sandbox: SandboxMode,
                  escalation_of: Option<DenialClass> },   // "on-failure" re-run unsandboxed
    ApplyPatch  { diff: UnifiedDiff, files: Vec<PathBuf> },
    GitMutation { description: String, command: Vec<String> },
    McpToolCall { server: String, tool: String, args: serde_json::Value },
    NetworkHost { host: String },                          // Phase-2 proxy hook, type ships now
    ExitPlanMode { plan: PlanState },
}
pub enum Decision { Approve, ApproveForSession, Deny, DenyWithFeedback(String), Abort }
pub enum ApprovalPolicy { Untrusted, OnRequest, OnFailure, Never }   // brief §3 matrix
```

### 2.3 SandboxPolicy (in protocol; compiled by `grokforge-sandbox`)

```rust
pub struct SandboxPolicy {
    pub mode: SandboxMode,                  // ReadOnly | WorkspaceWrite | DangerFullAccess
    pub writable_roots: Vec<PathBuf>,       // workspace, $TMPDIR, /dev/null
    pub readable_roots: Vec<PathBuf>,       // default: ["/"]
    pub unreadable_globs: Vec<String>,      // ["**/.env*", "**/*.pem", ...] — ledger/redaction aligned
    pub protected_paths: Vec<PathBuf>,      // [".git"] — deny-write even in workspace-write
    pub network: NetworkMode,               // Isolated | ProxyRouted{socket} | Full
}
// sandbox crate:
pub trait SandboxBackend: Send + Sync {
    fn capability(&self) -> SandboxCapability;                      // startup self-test result
    fn plan(&self, policy: &SandboxPolicy, cmd: &CommandSpec) -> Result<SandboxedCommand>;
    fn classify_failure(&self, exit: &ExitStatus, stderr: &[u8]) -> Option<DenialClass>;
}
```

### 2.4 The unified Tool trait (built-ins + MCP)

```rust
// grokforge-core::tools
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;             // name, description, JSON Schema params,
                                            // mutating: bool, parallel_safe: bool
    fn approval_need(&self, args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed;
                                            // Never | PerPolicy | Always(ApprovalKind)
    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput;
}
pub struct ToolInvocation<'a> {
    pub call_id: ToolCallId,
    pub args: serde_json::Value,
    pub ctx: &'a TurnContext,               // cwd, SandboxPolicy, session handles
    pub emit: EventEmitter,                 // ToolOutputDelta streaming
    pub ledger: LedgerHandle,               // record bytes if output enters context
    pub cancel: CancellationToken,
}
pub enum ToolOutput {
    Success { content: Vec<ContentBlock>, ui: Option<ToolUiHint> },
    Failure { error: String, denial: Option<DenialClass> },   // triggers on-failure escalation
}
// MCP adapter (grokforge-core, wrapping grokforge-mcp::McpConnection):
pub struct McpToolAdapter { server: String, conn: Arc<dyn McpConnection>, spec: ToolSpec }
// impl Tool for McpToolAdapter — approval_need = Always(McpToolCall{..}) unless allowlisted
pub struct ToolRegistry { tools: HashMap<String, Arc<dyn Tool>> }  // ≤200/request (xAI cap) enforced here
```

### 2.5 Session / Turn / history (core) and HistoryCell (tui)

```rust
// core
pub struct Session {
    id: SessionId,
    config: Arc<SessionConfig>,             // model routing, policies, workspace root
    history: ConversationHistory,           // Vec<ResponseItem>: canonical model-visible record
    tools: ToolRegistry,
    approvals: ApprovalBroker,              // ApprovalId -> oneshot::Sender<Decision>
    ledger: ContextLedger,
    store: StoreHandle,
    subagents: SubagentManager,
    phase: watch::Sender<TurnPhase>,
}
pub enum TurnPhase {
    Idle,
    Streaming,
    RunningTools { pending: HashSet<ToolCallId> },
    AwaitingApproval { requests: Vec<ApprovalId> },
    Compacting,
}
// protocol::items — what history is made of (persisted verbatim, replayed on resume):
pub enum ResponseItem {
    UserMessage(Vec<InputItem>),
    AssistantMessage { text: String },
    ReasoningSummary { text: String },      // kept out of resend by default (cache-friendly)
    ToolCall { id: ToolCallId, name: String, args: Value },
    ToolResult { id: ToolCallId, content: Vec<ContentBlock>, truncated: bool },
    CompactionSummary { text: String, replaces: Range<usize> },
}
// tui — presentation-only, derived from EventMsg, never persisted:
pub enum HistoryCell {
    User(UserCell), AgentMarkdown(StreamCell), Reasoning(CollapsibleCell),
    Tool(ToolCell /* header + capped output ring */), Diff(DiffCell),
    ApprovalResolved(BannerCell), Usage(UsageCell), Error(BannerCell),
}
// every cell: fn lines(&self, width: u16, theme: &Theme) -> &[Line]  — wrap-cached (render crate)
```

### 2.6 xAI client surface

```rust
// grokforge-xai
pub struct XaiClient { http: reqwest::Client, cfg: ProviderConfig }
impl XaiClient {
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, XaiError>;   // startup slug validation
    pub async fn stream(&self, req: ResponsesRequest) -> Result<ResponseStream, XaiError>;
}
pub struct ResponsesRequest {
    pub model: String, pub input: Vec<WireItem>, pub tools: Vec<WireTool>, // client + server-side mixed
    pub parallel_tool_calls: bool, pub reasoning: Option<ReasoningOpts>,   // effort knob
    pub response_format: Option<JsonSchemaFormat>, pub conv_id: Option<String>, // x-grok-conv-id
    pub store: bool,                       // false whenever images in context (vision caveat)
}
pub type ResponseStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, XaiError>> + Send>>;
pub enum StreamEvent {
    Created { response_id: String },
    OutputTextDelta(String),
    ReasoningSummaryDelta(String),
    ToolCallComplete(WireToolCall),        // arrives WHOLE per brief — no partial-JSON assembly
    ServerToolBegin { kind: ServerToolKind }, ServerToolEnd { kind: ServerToolKind, billed: bool },
    Usage(WireUsage),                      // input/cached/output/reasoning tokens
    Completed { stop: WireStopReason }, Failed { error: WireError },
}
```

---

## 3. Async Architecture

### 3.1 Task topology

```
main (grokforge bin, tokio multi-thread rt)
│
├─ TUI thread-side:
│   ├─ T1 input task:    crossterm EventStream ──► app_events mpsc
│   ├─ T2 app/draw loop: select!{ app_events, core_events(rx), commit_tick, resize }
│   └─ commit ticker:    adaptive interval (queue depth + staleness) → insert_before drains
│
├─ CORE (spawned by ConversationManager::spawn → (mpsc::Sender<Submission>, mpsc::Receiver<Event>)):
│   ├─ T3 submission loop (owns Session):
│   │     recv Op ─ UserTurn → spawn T4 (aborts pending? no: rejects if !Idle)
│   │              ─ Interrupt → turn_cancel.cancel()
│   │              ─ ApprovalDecision → approvals.resolve(id, decision)  (oneshot)
│   ├─ T4 TurnRunner (per turn, JoinHandle kept):
│   │     ├─ T5 SSE reader: XaiClient::stream → mpsc<StreamEvent> (bounded 128)
│   │     └─ T6..n tool tasks: JoinSet<(ToolCallId, ToolOutput)>
│   │            └─ exec: blocking PTY reader threads → mpsc, 50ms coalescing
│   ├─ Tstore: write-behind persistence task (mpsc<StoreOp>, batched tx per turn boundary)
│   └─ Tmcp: per-server supervision tasks (rmcp transports)
│
└─ Headless: replaces T1/T2 with a single "drain events → JSONL stdout" task; same core.
```

### 3.2 Channels

| Channel | Type | Bound | Direction |
|---|---|---|---|
| Submissions (SQ) | `mpsc::Sender<Submission>` | 64 | frontend → core |
| Events (EQ) | `mpsc::Sender<Event>` | 256 | core → frontend; **coalesced deltas** so a `yes \| head -c 100M` tool can't flood the render loop; if full, tool-output producers drop-merge chunks (never block the turn loop on the UI) |
| Approval resolution | `oneshot::Sender<Decision>` per ApprovalId | — | inside core (broker) |
| SSE internal | `mpsc<StreamEvent>` | 128 | T5 → T4 |
| Store | `mpsc<StoreOp>` | 512 | core → Tstore (write-behind; flushed synchronously at TurnComplete) |
| Phase | `watch::Sender<TurnPhase>` | — | core → frontends (status line, spinner) |

### 3.3 Cancellation (Esc)

`tokio_util::sync::CancellationToken` hierarchy: `session_token → turn_token → per-tool child tokens`.

Esc in TUI → `Op::Interrupt` → submission loop calls `turn_token.cancel()`:
1. T5 SSE reader: `select!` on token → drops the reqwest stream (aborts the HTTP request).
2. Tool tasks: each `ToolInvocation.cancel` fires → `exec` sends SIGTERM to the **process group**, SIGKILL after 2s grace; PTY masters closed; MCP calls get rmcp cancellation notification.
3. ApprovalBroker resolves all pending oneshots with `Decision::Abort`.
4. TurnRunner drains the JoinSet (bounded 5s), appends partial tool results as `ToolResult{ content: "[interrupted]" }` to history (so the model sees a coherent transcript on the next turn), emits `TurnComplete{ stop: Interrupted }`, phase → Idle.

Double-Esc within 1s = also clears the composer; Ctrl-C twice = `Op::Shutdown` (graceful: store flush, MCP servers killed, terminal restored via panic-hook-safe guard).

### 3.4 SSE → render pipeline handoff

```
xAI SSE bytes ─ eventsource-stream ─► StreamEvent (T5)
  ─► TurnRunner: map to EventMsg::{AgentMessageDelta, ReasoningSummaryDelta} (T4)
  ─► EQ ─► TUI app loop: append raw source to active cell's StreamController (render crate)
        StreamController: newline-gated commit queue
          ├─ stable region ──(commit tick)──► pulldown-cmark render → wrap-cache → insert_before → native scrollback
          └─ live tail ──(each frame, throttled)──► mutable bottom of viewport
        holds back: tables, unclosed code fences (layout may reshape; scrollback is immutable)
```
Never re-parse/re-wrap committed cells per token; re-wrap only on resize; syntect cache by `(lang, hash)`; every draw wrapped in DEC-2026 synchronized output.

---

## 4. Session Persistence

SQLite (rusqlite, bundled), one DB at `~/.grokforge/sessions.db`, WAL, `busy_timeout=5s`, `user_version` migrations.

```sql
CREATE TABLE sessions (
  id TEXT PRIMARY KEY, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  title TEXT, workspace_root TEXT NOT NULL, model TEXT NOT NULL,
  config_json TEXT NOT NULL,                 -- frozen SessionConfig snapshot
  parent_session_id TEXT REFERENCES sessions(id),   -- fork lineage
  forked_at_turn INTEGER,                    -- fork point (turns.idx in parent)
  status TEXT NOT NULL DEFAULT 'active'      -- active|archived
);
CREATE TABLE turns (
  id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  idx INTEGER NOT NULL, mode TEXT NOT NULL,  -- execute|plan
  started_at INTEGER, ended_at INTEGER, stop_reason TEXT,
  UNIQUE(session_id, idx)
);
CREATE TABLE items (                          -- canonical model-visible transcript (ResponseItem)
  id TEXT PRIMARY KEY, turn_id TEXT NOT NULL REFERENCES turns(id),
  seq INTEGER NOT NULL, kind TEXT NOT NULL,   -- user|assistant|reasoning|tool_call|tool_result|compaction
  payload_json TEXT NOT NULL,
  UNIQUE(turn_id, seq)
);
CREATE TABLE tool_calls (
  id TEXT PRIMARY KEY, turn_id TEXT NOT NULL REFERENCES turns(id),
  name TEXT NOT NULL, args_json TEXT NOT NULL,
  status TEXT NOT NULL,                       -- ok|failed|denied|interrupted
  approval_decision TEXT, sandbox_mode TEXT, denial_class TEXT,
  exit_code INTEGER, started_at INTEGER, ended_at INTEGER,
  output_bytes INTEGER, output_spill_path TEXT  -- >64KB outputs spill to capped files, tail kept inline
);
CREATE TABLE usage (
  turn_id TEXT NOT NULL REFERENCES turns(id), request_seq INTEGER NOT NULL,
  model TEXT NOT NULL, input_tokens INTEGER, cached_tokens INTEGER,
  output_tokens INTEGER, reasoning_tokens INTEGER,
  server_tools_json TEXT,                     -- per-kind counts (web_search/x_search/...) — $5–10/1k metering
  cost_usd REAL, PRIMARY KEY(turn_id, request_seq)
);
CREATE TABLE ledger (                          -- context ledger: what left the machine
  id INTEGER PRIMARY KEY, session_id TEXT NOT NULL, turn_id TEXT,
  source TEXT NOT NULL,                       -- file path | 'repo_map' | 'agents_md' | tool name
  bytes INTEGER NOT NULL, redactions_json TEXT, reason TEXT, created_at INTEGER
);
CREATE TABLE compactions (
  session_id TEXT NOT NULL, turn_id TEXT NOT NULL,
  summary_item_id TEXT NOT NULL REFERENCES items(id),
  replaced_from_seq INTEGER, replaced_to_seq INTEGER
);
```

**Resume:** `grokforge resume [id|--last]` → load session row, replay `items` in `(turns.idx, seq)` order into `ConversationHistory`, applying compaction records (summary item substitutes the replaced range). Tool outputs spilled to disk are re-truncated on load. Startup re-validates model slug against `/v1/models`; if retired, prompt to remap (config data, not constant).

**Fork:** `grokforge fork <id> [--at-turn N]` → new session row with `parent_session_id`/`forked_at_turn`; items up to the fork point are **copied** (simple, correct; content-addressed dedupe is a later optimization). Subagent sessions are ordinary sessions with `parent_session_id` = spawner and a `task:` title prefix — same tables, free persistence.

**Debug logs:** `tracing` with a custom size-capped rolling appender in `grokforge-store::logs` (default 5 MB × 5 files, checked on every write). Event payloads logged at `debug` are truncated to 2 KB. This is the anti-codex-640TB guarantee, enforced in one place.

---

## 5. The Agent Loop

### 5.1 Turn state machine (TurnRunner)

```
Idle
 └─ Op::UserTurn ─► AssembleContext
      ├─ stable prefix (cache-aligned, §5.3): system prompt · AGENTS.md chain · tool specs · repo map · skills index
      ├─ history (post-compaction) · @-mention file contents (via ledger+redactor) · user items
      ▼
 Streaming  ── XaiClient::stream, T5
      ├─ text/reasoning deltas ─► EventMsg deltas
      ├─ StreamEvent::Failed / disconnect ─► retry w/ backoff (idempotent: no side effects yet) ─► Streaming
      └─ Completed:
           ├─ no tool calls, stop=end_turn ────────────────────────► Finalize
           └─ tool calls (whole, possibly parallel) ─► DispatchTools
                ├─ per call: registry lookup ─► approval_need × ApprovalPolicy × SandboxMode matrix
                ├─ auto-approved & parallel_safe ─► JoinSet (cap 4; reads parallel,
                │      mutating fs/git tools serialized behind an async write-lock)
                ├─ needs approval ─► AwaitingApproval (ApprovalRequested events; other tools keep running)
                │      Decision::Approve/ApproveForSession ─► JoinSet
                │      Decision::Deny(WithFeedback) ─► synthesize ToolResult("denied: …")
                └─ ToolOutput::Failure{denial: Some(class)} + policy==OnFailure
                       ─► classifier ─► escalation ApprovalRequested(ExecCommand{escalation_of}) 
                       ─► approved: re-run outside sandbox (host) ─► result
      ▼
 all results appended as ResponseItem::ToolResult ─► iteration++ 
      ├─ iteration < max (default 32) ─► Streaming            (the loop)
      └─ else ─► Finalize(MaxIterations)
      ▼
 Finalize: token check ─► maybe Compacting (§5.4) ─► git auto-commit (host, if enabled)
           ─► store flush ─► TokenUsage + TurnComplete ─► Idle
 (any state) turn_token cancelled ─► unwind per §3.3 ─► Finalize(Interrupted)
```

Key signature:

```rust
impl TurnRunner {
    pub async fn run(mut self, input: UserTurnInput, cancel: CancellationToken)
        -> Result<TurnOutcome, TurnError>;
    async fn dispatch_tools(&mut self, calls: Vec<WireToolCall>, cancel: &CancellationToken)
        -> Vec<(ToolCallId, ToolOutput)>;   // parallel JoinSet + approval interleaving
}
```

### 5.2 Parallel tool calls

xAI sends `parallel_tool_calls` on by default and each call arrives whole, so dispatch is: partition → approval-gate → `JoinSet` with concurrency cap 4. Ordering guarantee to the model: results are appended to history in the **original call order** regardless of completion order. `spec().parallel_safe == false` (write_file, edit, apply_patch, git_commit, shell-in-workspace-write) serialize behind a per-session `tokio::sync::Mutex`; reads (read_file, grep, glob, git_diff, MCP reads) run concurrently.

### 5.3 Context management (what enters the prompt)

Assembled by `core::context::ContextAssembler`, **every byte ledgered** (§6.3):
1. **System prompt** (versioned, in-binary) + sandbox/approval mode description so the model knows its constraints.
2. **AGENTS.md chain**: `~/.grokforge/AGENTS.md` → repo root `AGENTS.md` → nearest-ancestor-of-cwd `AGENTS.md` (concatenated with provenance headers).
3. **Tool specs** (≤200; MCP tools namespaced `mcp__server__tool`), plus server-side tool mixins (`web_search`, `x_search`) when enabled in config.
4. **Repo map** (`grokforge-index`): token-budgeted (default 2k tokens, config), rank seeded by files mentioned/edited this session; **refreshed only when the seed set or repo hash changes** — it sits in the stable prefix, so churning it kills prompt-cache hits (cached input is 4–6× cheaper).
5. **Skills index**: one-line description per discovered skill (`.grokforge/skills/*/SKILL.md` frontmatter); the model requests full skill bodies via `read_file` (lazy loading).
6. **History** post-compaction, then **@-mentions** (full file or range, through the redactor), then the user message.

Prefix ordering is **stability-sorted** (most-stable first) for xAI automatic prefix caching; `x-grok-conv-id` sent per session; `store: false` forced when any image is in context.

Model routing (config data): default loop `grok-build-0.1` (effort from config), plan mode `grok-4.5` high, `/model` mid-session switch is first-class (history is model-agnostic `ResponseItem`s).

### 5.4 Compaction

Trigger: post-turn tokens > 80% of the active model's context (from `ModelInfo`), or `Op::Compact`.
- Summarize `items[compaction_floor..len-K]` (K = keep-tail, default last 2 turns) via a dedicated request with `response_format: json_schema`:
  ```json
  { "summary": "...", "files_touched": ["verbatim/paths.rs"], "errors_verbatim": ["exact error text"],
    "open_tasks": [...], "key_decisions": [...] }
  ```
- **Preserve-verbatim rule (brief):** `files_touched` paths and `errors_verbatim` strings are extracted **mechanically** from tool_call/tool_result items (not model-generated) and injected into the summary item untouched — the model summary covers narrative only. Plan state (`PlanState`) is re-pinned after the summary, never summarized away.
- Replaced range recorded in `compactions`; original items stay in SQLite (resume shows full transcript; only the *model-visible* window shrinks). The stable prefix (system/AGENTS.md/repo map) is never compacted — it's the cache anchor.

---

## 6. Plan Mode, Subagents-in-Worktrees, Context Ledger — hook points

### 6.1 Plan mode
- `Op::UserTurn{ mode: Plan }` → `TurnContext.mode = Plan`: `ToolRouter` filters the registry to read-only tools (`spec().mutating == false`) + `update_plan`; sandbox forced `ReadOnly`; model routed per config (`grok-4.5`, effort high).
- `update_plan` tool (json_schema-validated `PlanState { steps: Vec<PlanStep{ text, status }> }`) → `EventMsg::PlanUpdated` → TUI plan widget; persisted as an item.
- Exit: model calls `exit_plan_mode` → `ApprovalKind::ExitPlanMode{plan}` bottom-pane approval → on approve, session flips to Execute, `PlanState` pinned into the context tail (survives compaction per §5.4), sandbox restored to configured mode. This is entirely a policy overlay on the existing turn machinery — no new loop.

### 6.2 Subagents in worktrees
- `core::subagent::SubagentManager` (core-resident tool, §1 cycle note). Tool `spawn_task { prompt, model?, isolated: bool }`:
  1. Host process (trusted) calls `grokforge-git::worktree::create(task_id)` → `.grokforge/worktrees/<task-id>` on branch `grokforge/task/<task-id>`.
  2. Spawns a child `Session` (same `ConversationManager`, `parent_session_id` set) with a **narrowed** `SandboxPolicy` (`writable_roots = [worktree]`, network per parent), depth cap 1, concurrency cap 8 (brief table stakes), a child `CancellationToken` under the parent turn's token (parent Esc kills all tasks).
  3. Child events are re-emitted wrapped as `EventMsg::SubagentEvent{task_id, ..}` — TUI renders a collapsed task cell with live status; headless emits them as JSONL with a `task_id` field.
  4. Completion: tool output = final message + `git diff --stat` of the worktree; merge-back is a **separate approval-gated host git mutation** (`ApprovalKind::GitMutation`) — rebase onto the main workspace or extract as a patch; worktree removed after merge/discard.
- Architecture cost is near-zero because a subagent *is* a `Session` — persistence, compaction, ledger, cancellation all come for free.

### 6.3 Context ledger (the marketing wedge — must be airtight)
- **Single choke point:** `ContextAssembler` is the *only* code path that constructs `ResponsesRequest.input`; `XaiClient` accepts requests only via a `LedgeredRequest` wrapper that core can only mint through the assembler (enforced by type privacy: the constructor is `pub(crate)` in core). Nothing reaches the wire un-ledgered.
- Every source (file content, repo map, AGENTS.md, tool result, @-mention, image) passes `Redactor::apply(source, bytes) -> (bytes, Vec<Redaction>)` — `.env*`/key-file globs (aligned with `SandboxPolicy.unreadable_globs`) and secret-pattern regexes, on by default — then records `LedgerEntry { source, bytes, redactions, reason }` → `EventMsg::LedgerAppended` + `ledger` table.
- TUI `ledger_panel`: per-request and per-session bytes-by-source, redaction count, server-side tool call meter with $ (from `usage.server_tools_json`), cached-token savings. Headless: `--show-ledger` adds ledger records to JSONL.
- Zero telemetry, period; the ledger doubles as the audit log (positioning gap #1 vs Grok Build's GCS-upload scandal).

---

## 7. Build & Sequencing (implementation order)

1. **Foundations:** `protocol` → `config` → `xai` (mock-SSE integration tests) → `store`.
2. **Vertical slice (headless first):** `core` minimal turn loop (no tools) → `headless` JSONL — proves the Op/Event boundary before any TUI exists.
3. **Safety spine:** `sandbox` (Linux backend + classifier + self-test) → `exec` (+ `__sandbox-helper` arg0 in `cli`) → macOS backend → Windows WSL2 delegation.
4. **Tools & git:** `tools` built-ins, `git` (reads → autocommit/undo → worktrees), approval broker + policy matrix.
5. **TUI:** `render` (snapshot-tested pipeline) → `tui` (composer, chatwidget, approval modal, ledger panel).
6. **Differentiators:** `index` repo map → plan mode → `mcp` → subagents → compaction polish → skills/plugins conventions.
7. **Release:** cargo-dist, cargo-binstall, macOS sign/notarize, tmux/Zellij/SSH CI matrix, insta snapshots, `grokforge debug sandbox`.

Cross-cutting risks carried from the brief into structure: rmcp churn isolated in `mcp`; ratatui-widget churn avoided by owning composer/markdown (`render`, `tui::composer`); fastembed's ort pin feature-gated off in `index`; sandbox degradation always surfaced (`SandboxCapability` in `SessionConfigured`); logs size-capped in exactly one module.

### Critical Files for Implementation
- /Users/intzero/Documents/GrokForge/crates/protocol/src/lib.rs — Op/Event/ApprovalRequest/SandboxPolicy: the contract every other crate compiles against; freeze first.
- /Users/intzero/Documents/GrokForge/crates/core/src/turn.rs — TurnRunner state machine (stream → tools → approvals → loop), cancellation unwind, compaction trigger.
- /Users/intzero/Documents/GrokForge/crates/core/src/context.rs — ContextAssembler + Redactor + ledger choke point (the privacy guarantee lives or dies here).
- /Users/intzero/Documents/GrokForge/crates/xai/src/stream.rs — /v1/responses SSE decode into typed StreamEvent, reconnect/backoff, whole-chunk tool calls.
- /Users/intzero/Documents/GrokForge/crates/sandbox/src/linux.rs — landlock+seccompiler backend, `apply_in_child`, RulesetStatus surfacing, denial classifier hooks.