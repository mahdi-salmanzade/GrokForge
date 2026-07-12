# Changelog

All notable changes to GrokForge are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- M0 scaffold: Cargo workspace with 12 crates, dual `MIT OR Apache-2.0` licensing,
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
- M6 git-native workflow (`grokforge-git` + core wiring): every mutating turn's edits become a
  real commit from the trusted host process (never in the sandbox). `Git` shells out to the
  `git` CLI for discovery/status/commit/undo/worktrees. Auto-commit stages **only the paths the
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
- TUI slash commands: `/help`, `/plan <task>`, `/undo` (host-side git undo of the session's agent
  commits), `/clear`, `/quit`.
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
  per-target binaries on tag. (macOS signing/notarization + Homebrew tap wired but pending
  credentials.)
