# Adversarial Review: GrokForge v0.1 Designs vs Brief

## 1. CONTRADICTIONS

1. **[BLOCKER] Canonical persistence format — A vs B vs C.** Design A makes SQLite (`~/.grokforge/sessions.db`, `items` table) the canonical transcript, with resume replaying rows. Design B makes append-only JSONL rollouts (M2) canonical, with SQLite as a rebuildable *index* (M8, "sqlite index rebuilds from JSONL if deleted"). Design C says "SQLite session store (size-capped, rotating)" — rotating session storage would silently destroy resumable history. These are three incompatible architectures and M2 cannot start until one is picked. **Fix:** adopt B — JSONL append-only rollout per session from M2 (best debugging/test-fixture artifact, matches the early-recording rationale), SQLite as a derived index at M8. Design A's schema shrinks to the index tables; "size-capped rotating" applies only to debug logs, never session data. **Applies:** A §4, B M2/M8, C §1.7.

2. **[BLOCKER] Privacy claim is falsifiable as written.** C's first-run promise ("the only network calls go to the API endpoint you configure") and B's M11 egress test ("only remote host contacted is the configured base URL") are contradicted by A's own design: the `web_fetch` built-in tool, rmcp streamable-HTTP MCP clients, and the built-in xAI Docs MCP (`docs.x.ai`) all egress from the grokforge process. As specced, either the M11 test fails or the marketing claim is false — exactly the gap the brief says HN will find. **Fix:** scope the claim to "no egress except endpoints you explicitly configure, every one visible in the ledger"; the egress test asserts against the *configured allowlist* (API base URL + enabled MCP/web_fetch endpoints) and asserts every contacted host has a ledger entry. Consider cutting `web_fetch` from v0.1 entirely (not in the brief; simplifies the claim). **Applies:** A §1 (tools), B M11, C §7.1.

3. **[MAJOR] Ledger choke-point location — A vs B/C.** A puts the choke point in `core::context::ContextAssembler` (type-privacy-enforced `LedgeredRequest`); B puts the "request-audit data plane" in the client (M1); C says "captured at the single serialization choke point in the xai client crate." Byte-exact reconciliation (B's M7 test) is only achievable at serialization time in the client; provenance ("which file, why") is only knowable in the assembler. **Fix:** both, explicitly layered — provenance + redaction in core's assembler, byte accounting at client serialization, one reconciliation invariant between them tested from M2 (see finding 22). **Applies:** A §6.3, B M1/M7, C §1.9.

4. **[MAJOR] Telemetry: C's first-run wizard step 5 offers a telemetry opt-in "with a precise list of what opt-in would send" — but A says "zero telemetry, period" and B's M11 test asserts "zero telemetry code paths exist."** You cannot ship an opt-in prompt for a capability that doesn't exist without either lying or failing B's test. **Fix:** delete C's wizard step 5; replace with a static privacy statement screen. Keep B's code-path audit. **Applies:** C §7.5, vs A §6.3 and B M11.

5. **[MAJOR] Auto-commit granularity — A/B vs C.** A ("every agent edit → commit") and B M6 ("auto-commit per agent edit") vs C §5.1 (two triggers: pre-command checkpoint + turn end, staging only agent-touched paths). Per-edit commits produce noise chains and sweep risk; C's design is strictly better thought out (checkpoint-before-command is the actual safety property). **Fix:** adopt C §5.1–5.3 verbatim (including trailers, `reset --keep` vs revert, backup refs); A's `autocommit` module and B's M6 deliverable text update to match. **Applies:** A §1 (git), B M6.

6. **[MAJOR] Crate layout — 16 crates (A) vs 10 (B) vs C's paths.** A: 16 crates + xtask; B: 10; C references `grokforge-sandboxing` (brief's name) while A/B use `grokforge-sandbox`, and C puts git in `grokforge-core/src/git/`. This must be settled at M0 since the scaffold is an M0 deliverable. **Fix:** ~11 crates: `protocol`, `config`, `xai`, `sandbox` (with `exec` folded in or kept if the arg0-helper link-size argument holds), `git`, `mcp`, `context` (A's `index` + `file-search` merged, per B), `core` (tools trait + built-ins + store module), `render` (keep separate — A's snapshot-testability justification is the strongest boundary argument in any doc), `tui`, `grokforge` bin (headless as a module, not a crate — 500 lines doesn't justify a crate), plus `test-support` (B; A omits it entirely despite mock-SSE being load-bearing in both). **Applies:** A §1, B §2, C critical files.

7. **[MINOR] Headless CLI surface.** A/B: `grokforge exec -p`; C: `grokforge -p` with `exec` explicitly reserved post-v0.1. Flags diverge (`--output json|text` vs `--json`; `--show-ledger` vs `--ledger <file>`; `--full-auto` vs `--preset`/`--allow`) and exit codes conflict (A: 0/2/3; C: 0/1/2/3/4/5/124/130). **Fix:** `grokforge exec -p` as canonical with top-level `-p` alias; adopt C's flag set and exit codes (richer, CI-grade); B's `--full-auto` becomes sugar for `--preset yolo` or is dropped. **Applies:** A §1 headless/cli, B M2, C §6.

8. **[MINOR] Headless approval semantics.** A: "default deny + nonzero exit"; B: "auto-denies or fails fast"; C: auto-deny with feedback, turn continues, exit 5 only if fatal, `--allow` pre-grants boundaries. **Fix:** adopt C (deny-and-continue with `approval.auto` events); it's the only version that lets an agent route around a denial. **Applies:** A, B M2.

9. **[MINOR] Composer queueing vs core turn admission.** C promises "composer stays live (queue next message)" while A's submission loop "rejects if !Idle." **Fix:** TUI-side queue, submitted on `TurnComplete`; document in the protocol contract. **Applies:** A §3.1, C §1.2.

10. **[MINOR] Esc semantics collisions.** A: double-Esc clears composer; C: Esc-Esc (empty, idle) opens backtrack-fork picker. Also underspecified: with an approval modal open *while other tools still run* (A allows this), does Esc deny the one approval or interrupt the turn? **Fix:** single table in the merged spec: Esc = close popup > deny focused approval > interrupt turn; Esc-Esc idle+empty = backtrack (C); composer clearing stays on Ctrl+C. **Applies:** A §3.3, C §3.3.

11. **[MINOR] ACP — all three designs defer it, but the brief (§4.5) lists ACP among the table stakes to match, and the user lock is "full differentiator v0.1 scope."** A shared, silent deviation from the brief. Deferring is probably right (protocol crate is the hedge; v0.1 is already enormous), but it must be an explicit ADR, not an accident in three documents. **Applies:** A, B §5, C §8.

12. **[MINOR] B conflates client-side subagents with the multi-agent model.** M10 adds "RPS throttling for tier-gated multi-agent model (grok-4.20…)" to the subagent milestone — but worktree subagents run the default loop model; `grok-4.20-multi-agent` is a distinct server-side deep-research role in the brief. **Fix:** subagents route to `grok-build-0.1`; drop the multi-agent model from M10 (defer the deep-research role entirely). **Applies:** B M10.

13. **[MINOR] Write-primitive set.** B: `apply_patch` is "the write primitive"; A ships `write_file` + `edit` (string-replace) + `apply_patch`. **Fix:** pick `edit` (string-replace) + `write_file` as primary (best empirical model compliance), `apply_patch` optional; whichever is chosen, C's `✎ edit` cell and the diff-first `PatchProposed` flow must work for all of them. **Applies:** A §1 tools, B M2.

14. **[MINOR] State directories.** A: `~/.grokforge/{config.toml,sessions.db,logs,cache}`; B: `~/.local/share/grokforge/sessions/`. **Fix:** XDG-compliant (`directories` crate conventions) decided at M0. **Applies:** A, B.

## 2. GAPS (no design covers)

15. **[MAJOR] No tokenizer / token-counting strategy anywhere.** The 80% compaction trigger (A §5.4), 2k-token repo-map budget (A §5.3), `ctx 38% (97k/256k)` status segment (C), and "token budgeting/truncation" (B M2) all require counting tokens locally, and no document says how — xAI's tokenizer isn't bundled. **Fix:** use `usage` from the previous response as ground truth for history size + a bytes/~4 heuristic for newly assembled content; document the error bar; probe whether xAI exposes a tokenize endpoint in the M1 nightly smoke. **Applies:** all three.

16. **[MAJOR] No pricing-data source for the cost display.** Cost-in-USD appears everywhere (A `CostBreakdown`/`cost_usd`, C `$0.41`, first-run `$/M` table) but nothing establishes where prices come from — `GET /v1/models` returning pricing is unverified, and the brief warns retired slugs silently *re-price*. **Fix:** ship a price table as config data with user override and an honest "price unknown" state; nightly live smoke diffs it against reality. **Applies:** A §2.1/§4, B M1, C §2 `/model`, §7.3.

17. **[MAJOR] Mid-stream SSE failure semantics underspecified.** A retries only "idempotent: no side effects yet"; B tests "mid-stream disconnect + backoff resume" — but `/v1/responses` SSE has no documented resume token, so "resume" of a half-streamed response is not a thing. What happens to partial text already committed to *immutable native scrollback*? **Fix:** define the policy: on mid-stream failure, mark the partial cell as aborted (visible banner), retry the whole request N times with backoff (prefix-cache makes replay cheap), and only then surface `Error{recoverable}`; C needs the `StreamRetrying` UX (it renders nothing for A's existing event). **Applies:** A §5.1, B M1 exit criteria, C §1.1.

18. **[MAJOR] Compaction has no milestone.** A specs it in detail (§5.4, preserve-verbatim, structured summary); C has `/compact`; B's roadmap never schedules it — M2 has only "token budgeting/truncation" and no later milestone mentions compaction or its tests. On long agentic sessions this is a day-one need. **Fix:** basic truncation M2; full structured compaction + mechanical preserve-verbatim extraction + `/compact` lands with M7 (context engine), with a test that verbatim paths/errors survive. **Applies:** B.

19. **[MAJOR] WSL2 delegation mechanics are hand-waved.** A rewrites commands to `wsl.exe -d <distro> -- grokforge __sandbox-helper …` — which requires a *Linux* grokforge binary already installed inside the distro (the Windows .exe cannot apply landlock). No design covers: getting the helper into WSL, distro selection, `/mnt/c` I/O performance (atrocious for repos on the Windows drive), or version skew between host and in-WSL helper. B explicitly punts real WSL2 e2e to manual testing, so this ships unverified. **Fix:** simplest honest v0.1: Windows native = approval-only (no sandbox), plus documentation that says "run grokforge inside WSL for the full Linux sandbox." Command-level delegation moves to Phase 2 with a bundled static musl helper. **Applies:** A §1 sandbox, B M5, C §4.4.

20. **[MINOR] Mid-session model retirement (410) unhandled.** All designs validate slugs at startup; none specifies runtime behavior when a slug 404/410s or a server-side tool dies mid-session (the exact failure that broke grok-cli). **Fix:** error taxonomy maps 404/410 → TUI auto-opens `/model` remap picker; headless exits 3 with a machine-readable reason. **Applies:** A xai `error`, C §2.

21. **[MINOR] Composer-pasted secrets bypass redaction.** Redaction covers file reads and tool output; nothing runs the redactor over outbound *user-typed/pasted* text (paste pills make pasting a whole `.env` one keystroke). **Fix:** run the redactor over all outbound input items, or explicitly document the exclusion in the ledger panel. **Applies:** A §6.3, B M2, C §1.9.

22. **[MINOR] Ledger reconciliation arrives too late to be cheap.** The byte-exact test is M7 (Lane C) but the substrate is M1 and requests start flowing at M2 — five milestones of drift risk before the invariant is checked. **Fix:** a miniature reconciliation assertion lives in the M2 mock-SSE integration suite from day one; M7 only adds the UI. **Applies:** B M2/M7.

23. **[MINOR] Prompt-injection posture absent.** `web_search`/`x_search`/MCP outputs are untrusted content feeding an agent with write+exec tools; no design even mentions it. v0.1 minimum: system-prompt hardening note + the approval matrix as the stated mitigation, documented honestly in the README threat model. **Applies:** all three.

24. **[MINOR] Reasoning-effort validity per model unverified.** The brief flags grok-4.3's effort "none" as unverified; C's commit-message call specs "effort low/none" and `grok-build-0.1` has *always-on* reasoning (does the effort knob apply at all?). **Fix:** M1 nightly probes effort acceptance per model; commit-msg call falls back gracefully. **Applies:** C §5.2, B M2.

## 3. OVERENGINEERING

25. **[MAJOR] A's 16-crate workspace.** `file-search` (tiny), `headless` (~500 lines), `tools`, `store`, and `exec` as separate crates each add workspace/CI/versioning overhead a small team pays forever; the dependency-inversion gymnastics for `tools` (registry assembled in `cli`, `spawn_task` exiled to core to dodge a cycle) is a symptom. **Fix:** finding 6's ~11-crate layout. Keep `render` and `protocol` — those splits earn their keep. **Applies:** A §1.

26. **[MINOR] C's vim mode in v0.1** — operators, registers, counts in a custom composer is a sub-project. The brief lists vim as an *optional* composer capability, not a v0.1 differentiator. **Fix:** defer entirely, or ship motions-only behind a config flag. **Applies:** C §3.4.

27. **[MINOR] C's subagent merge-conflict resolution UI** (`[k]eep mine / [t]ake agent / [e]dit later` per file) is a mini merge tool. **Fix:** v0.1 halts on conflict with the diff shown and "resolve manually, then `/agents m` again." **Applies:** C §5.6.

28. **[MINOR] A's `Op::ExecStdin`** (interactive stdin into running PTY tools) is scope creep with no UX in C to drive it. **Fix:** drop from v0.1 protocol; add later (protocol is append-only anyway). **Applies:** A §2.1.

29. **[MINOR] C's composer long tail** — queued-message chips, draft token-size indicator, `Ctrl+X Ctrl+E` $EDITOR, Esc-Esc backtrack fork — each small, collectively weeks. None is a differentiator. **Fix:** mark all [post-v0.1] except input history + kill ring + paste pills. **Applies:** C §1.2/§3.3.

## 4. SEQUENCING ERRORS

30. **[MAJOR] A's build order lacks B's most important correction: the sandbox *seam* (passthrough `SandboxPolicy` path) must exist the day the exec tool exists.** A's step order (core/headless step 2, sandbox step 3) doesn't state it, and A's exec tool would grow around raw `Command`. **Fix:** merged plan adopts B's M2 passthrough-backend rule verbatim; A's `sandbox::policy` types land with the protocol crate. **Applies:** A §7.

31. **[MINOR] B's dogfood gate (post-M4) precedes the real sandbox (M5)** — the team dogfoods a tool whose default `auto` preset claims "sandboxed, network off" while running a passthrough backend. Normalizing that is exactly brief risk #8 culture-side. **Fix:** dogfood in `strict` preset until M5-Linux lands, then flip. **Applies:** B M4/M5.

32. **[MINOR] B M8 exit criterion "kill TUI mid-turn → resume … continues the turn" is unverifiable-as-stated and overpromised** — in-flight SSE and half-run tool calls can't be resurrected from a JSONL log. **Fix:** restate: resume restores transcript to the last coherent item boundary, marks the interrupted turn, and the user re-prompts. **Applies:** B M8.

33. **[MINOR] Plan mode sits at M10 behind subagents, but per A §6.1 it's a cheap policy overlay** (tool filter + sandbox ReadOnly + model route) needing only M2+M5. Landing it early de-risks a headline feature and improves dogfooding. **Fix:** split M10: plan mode right after M5; subagents stay post-M6. **Applies:** B M10.

## 5. FEASIBILITY

34. **[MAJOR] Total v0.1 scope vs "small team."** B's own ledger (S1, M6, L4, XL1) with the XL on the critical path is optimistic even before C's UX long tail and A's 16 crates. The locked differentiators can't be cut, so the slack must come from findings 19 (Windows honesty), 25–29 (trims), and 33 (resequencing). State an explicit non-goal list in the README from M0 so scope pressure has somewhere to vent. **Applies:** all three.

35. **[MINOR] Whole-chunk tool-call arrival is a single-source brief claim hard-coded into A's client** (`ToolCallComplete`, "no partial-JSON assembly"). If wrong for large args or changed by xAI, the client breaks at its most critical path. **Fix:** keep a trivial partial-JSON accumulator behind the same `ToolCallComplete` event (fires on completion either way); nightly live smoke covers it. **Applies:** A §2.6, B M1.

36. **[MINOR] CI kernel assumptions unverified.** B's M5 asserts per-ABI `RulesetStatus` on GitHub runners — verify Landlock is actually in the boot LSM list on ubuntu-22.04/24.04 hosted runners and that `sandbox-exec` behaves inside macOS CI VMs *before* building the exit criteria around them (fallback: self-hosted or container-with-privileges job). **Applies:** B M5/§3.

---

## Synthesis Guidance

Merge as **one plan with three layers, each taking its strongest document as the base:**

1. **Architecture layer = Design A**, with the crate count cut to ~11 (finding 6), `test-support` imported from B, `ExecStdin` dropped, and the ledger split into client-side byte accounting + core-side provenance (finding 3). A's protocol types, async topology, turn state machine, and compaction spec are the best material in any document — keep them nearly verbatim.
2. **Roadmap layer = Design B**, amended: persistence decision (JSONL-canonical) written into M2; a compaction milestone added; plan mode pulled forward out of M10; Windows downgraded to approval-only + "run inside WSL" docs (M5 shrinks); mini ledger-reconciliation test moved to M2; dogfood gate runs `strict` until M5. B's lane structure, risk-retirement table, and CI design survive intact.
3. **UX layer = Design C**, trimmed: delete the telemetry wizard step, defer vim/merge-conflict-UI/composer extras, adopt C's git semantics (§5) and headless contract (§6, including exit codes) as canonical over A/B's sketches, and add the missing `StreamRetrying`/410-remap surfaces.

**Before any code: five ADRs** — (a) JSONL-canonical persistence, (b) ledger choke-point layering, (c) final crate map + naming (`grokforge-sandbox`, XDG dirs), (d) privacy claim wording + egress-test allowlist semantics (the wedge must be exactly as strong as the test), (e) ACP explicitly deferred with the protocol crate named as the hedge. Everything else in the three documents is reconcilable by the priority rule: A wins on internals, B wins on order, C wins on anything the user sees.