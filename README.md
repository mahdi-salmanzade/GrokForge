<div align="center">
  <img src="assets/grokforge.svg" alt="GrokForge" width="96" height="96" />
  <h1>GrokForge</h1>
  <p><strong>Open-source terminal coding agent for Grok — local-first, sandboxed, git-native.</strong></p>
</div>

---

> **Status: pre-release (v0.1 in progress).** GrokForge is being built in the open. Interfaces will change until the first tagged release.

GrokForge is a terminal AI coding agent that uses xAI's Grok models to read, edit, and run code in your project — with three things the alternatives don't give you together:

- **You see every byte that leaves your machine.** The **context ledger** shows exactly which files and how many bytes go to the API on every request, with secret redaction on by default. No telemetry. Bring your own API key.
- **Commands run in an OS-native sandbox.** Filesystem writes are confined to your workspace and network is denied by default, enforced by the kernel (Landlock + seccomp on Linux, Seatbelt on macOS) — not by the honor system.
- **It works like git should.** Every change the agent makes is a real commit with a good message. `/undo` reverts cleanly. Subagents run in isolated worktrees.

## Why GrokForge

| | GrokForge | Grok Build (xAI) | grok-cli (superagent) | Aider |
|---|---|---|---|---|
| Open source | ✅ MIT OR Apache-2.0 | ❌ closed | ✅ | ✅ |
| Bring your own key (no subscription) | ✅ | ❌ | ✅ | ✅ |
| See exactly what's sent (context ledger) | ✅ | ❌ | ❌ | ❌ |
| OS-native cross-platform sandbox | ✅ | partial | macOS only | ❌ |
| Git-native workflow | ✅ | ✅ | partial | ✅ |
| Native `x_search` / live X data | ✅ | ✅ | ❌ | ❌ |
| Actively maintained | ✅ | ✅ | ~ | ❌ (since Aug 2025) |

## Sandbox capability by platform

We state this plainly rather than overpromising:

| Platform | v0.1 enforcement |
|---|---|
| **Linux** (kernel ≥ 5.13) | Landlock filesystem confinement + seccomp network-deny, in-process. Degradation is surfaced, never silent. |
| **macOS** | Seatbelt (`sandbox-exec`) profile generated per policy; startup self-test with graceful fallback. |
| **Windows** | Approval-only in v0.1. **Run GrokForge inside WSL2 for the full Linux sandbox.** Native Windows sandboxing is planned (see Roadmap). |

## Install

_Installers land with the v0.1 release. For now, build from source:_

```sh
git clone https://github.com/grokforge/grokforge
cd grokforge
cargo build --release
./target/release/grokforge --help
```

Requires a recent stable Rust toolchain (edition 2024).

## Quick start

```sh
export XAI_API_KEY=...          # or run `grokforge login` to store it in your OS keyring
grokforge                       # interactive TUI in the current repo
grokforge exec -p "fix the failing tests"   # headless, for scripts and CI
```

## Roadmap (planned — not yet shipped)

Multi-provider fallback (OpenAI/Anthropic/Ollama) · L7 domain-allowlist network proxy · native Windows sandboxing · semantic (embedding) code index · ACP editor embedding · voice / remote control. Anything above is a goal, not a claim.

## Non-goals for v0.1

Being a general multi-provider agent (we are Grok-first by design) · a GUI/desktop app · replacing your editor. See `docs/design/` for the full design record and `docs/decisions/` for architecture decisions.

## Security & privacy

GrokForge makes **no network calls except to endpoints you explicitly configure**, and every one is visible in the context ledger. MCP servers you connect are external processes; their egress is outside our audit scope and flagged as such. Content returned by web/X search and MCP tools is untrusted input to an agent with edit and shell tools — the approval workflow and sandbox are the mitigations. See `SECURITY.md` (coming with v0.1) for the threat model.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
