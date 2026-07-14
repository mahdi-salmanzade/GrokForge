<div align="center">
  <img src="assets/grokforge.svg" alt="GrokForge" width="96" height="96" />
  <h1>GrokForge</h1>
  <p><strong>Make Grok great in the terminal.</strong></p>
</div>

GrokForge exists because using Grok from a terminal should feel like a serious development tool, not a chat window wired to `sh`.

Today it can read and edit project files, inspect Git state, run commands, keep a session going, use project-defined workflows, and hand isolated work to subagents that run in parallel. When an action needs to cross a safety boundary, GrokForge stops and asks.

This is a pre-release project. The execution and safety paths are in place, and the adaptive TUI now exposes tool activity, approvals, reasoning, retries, token use, and privacy accounting. Automatic context selection, richer rendering, and distribution still need work.

## Why I built GrokForge

> I believe I should know exactly what code is running on my own machine. Anything less is
> complete BS.
>
> — [XBToshi](https://x.com/XBToshi/status/2076521420017045618?s=20)

I built GrokForge because, for my work, I believe Grok 4.5 is the best model available right
now—and I love using it. [xAI built Grok 4.5](https://x.ai/news/grok-4-5) specifically for coding,
agentic tasks, and knowledge work. I wanted to give that model family a serious terminal
environment without turning my own machine or repository into a black box. The code that reads
files, runs commands, handles credentials, changes Git state, and sends context to a model should
be code I can inspect.

### Why GrokForge is different

GrokForge is better when control matters:

- **Audit the whole thing.** GrokForge is an MIT-licensed Rust workspace you can inspect,
  compile, modify, and run yourself. It has no telemetry.
- **Account for model requests.** Every first-party model request body goes through the context
  ledger, which attributes source bytes, records redactions, and reconciles them to the serialized
  request.
- **See what the agent is doing.** Tool activity, reasoning, approvals, retries, token use,
  subagents, request-byte totals, and redaction totals stay visible in the TUI.
- **Fail closed by default.** Normally sandboxed commands use Seatbelt on macOS or validated
  Bubblewrap on Linux, with workspace-confined writes, protected Git metadata, isolated networking,
  and a scrubbed environment. If the requested confinement cannot be enforced, the command does not
  run.
- **Choose every extra capability.** Hosted web search, X search, and code interpreter stay off
  until enabled. Project MCP servers do not start without explicit trust.
- **Own your credentials.** API keys and OAuth tokens live in one documented, password-encrypted
  local file—not an opaque OS secret store. On native macOS and Linux, model-run commands cannot
  read it even in full-access mode.
- **Parallelize without sharing a workspace.** Up to 32 subagents can work concurrently in
  separate private Git worktrees with scoped, reviewable changes; GrokForge never silently merges
  them.
- **Keep workflows with the repository.** Skills and reusable slash commands live beside the code,
  and typing `/` opens the local Forge Deck without sending command templates to the model just for
  browsing them.

## What works today

- An adaptive, branded native Rust TUI with streaming conversations and human-readable tool activity
- Headless runs for scripts and CI
- File read, write, edit, list, glob, and grep tools, plus safe read-only Git status and diff
- Sandboxed shell commands with approval controls
- Project skills in `.grokforge/skills/*/SKILL.md` and reusable slash commands in `.grokforge/commands/*.md`
- Explicit opt-ins for Grok web search, X search, and code interpreter tools
- Approval-gated MCP tools from reviewed project configuration via `--trust-project-mcp`
- Read-only plan mode
- Persistent sessions with resume support
- Parallel subagents (up to 32 per turn) in isolated worktrees with scoped commits, shown live in a "PARALLEL AGENTS" panel
- A context ledger that accounts for the request body sent to Grok
- Secret redaction, bounded output, and no telemetry

## Build it

You need Rust 1.88 or newer.

```sh
git clone https://github.com/mahdi-salmanzade/GrokForge.git
cd GrokForge
cargo build --release
```

## Signing in

Just run it — GrokForge sets up credentials on the first interactive launch when
`XAI_API_KEY` is not already set:

```sh
./target/release/grokforge
```

On that first launch:

1. Set and confirm a GrokForge password of at least 12 characters.
2. Choose how you want to connect:
   - **[1] Your Grok subscription** (SuperGrok / X Premium+) — signs in through your browser
     (OAuth); usage bills against your subscription, no API key needed. *xAI currently limits
     subscription API access to the SuperGrok **Heavy** tier; other tiers may get a 403 until xAI
     lifts that.*
   - **[2] An xAI API key** — paste a key from
     [console.x.ai](https://console.x.ai) (pay-as-you-go; new developer accounts also get free
     monthly credits via the data-sharing program).

GrokForge stores the active API key or OAuth tokens in `~/.grokforge/credentials.enc`. A fresh
random salt is combined with your password through Argon2id to derive the encryption key; a fresh
random nonce and ChaCha20-Poly1305 then encrypt and authenticate the credential payload. On Unix,
the file is restricted to the owner with `0600` permissions. The password itself is not stored.

Choosing a new login method replaces the previous one, so switching to subscription OAuth cannot
silently keep billing an older API key. When the native macOS or Linux sandbox is active, the
encrypted credential file is also masked from model-run commands—even in full-access mode.

On later runs, enter the same password to unlock the file. An incorrect password—or a modified or
corrupt ciphertext—is rejected because authenticated decryption fails. When subscription tokens
expire and a refresh token is available, GrokForge refreshes them and seals the updated tokens with
the same password.

Earlier builds delegated credential storage to the OS keychain. GrokForge no longer calls any OS
keychain or system secret-store API; this avoids the recurring macOS Keychain-access prompts and
makes the credential location and unlock behavior consistent across platforms. The tradeoff is that
there is no password recovery: if you forget it, remove the encrypted file and sign in again.

You can also set things up ahead of time:

```sh
grokforge login                 # store an xAI API key (password-encrypted on disk)
grokforge login --subscription  # sign in with your SuperGrok / X Premium+ subscription
export XAI_API_KEY=your_key     # or just use an environment variable (best for CI, no password)
```

Resolution order is `XAI_API_KEY` env → the encrypted file (unlocked with your password) →
interactive setup. The environment variable always wins and requires no password, which keeps CI
and other non-interactive runs usable. Run `grokforge doctor` to see which credential is active and
whether the sandbox is enforced.

Run one task without opening the TUI:

```sh
./target/release/grokforge exec -p "find the bug and explain the fix"
```

Enable Grok-hosted tools only when a task needs them. They are off by default and may be billed separately:

```sh
grokforge exec --web-search --x-search -p "research this dependency change"
grokforge exec --code-interpreter -p "analyze these benchmark results"
```

Inside the TUI, use `/help` to discover commands, `/skills` to inspect project guidance, and
`/tools` to view or toggle hosted tools for the current session. A project command such as
`.grokforge/commands/verify.md` becomes `/verify`.

Useful commands:

```sh
grokforge doctor
grokforge sessions
grokforge resume
grokforge exec --plan -p "plan the refactor"
# After reviewing .grokforge/mcp.json in a trusted project:
grokforge --trust-project-mcp
```

## Safety

macOS uses Seatbelt. Linux uses Bubblewrap 0.11.2 or newer; Ubuntu 24.04 also needs an AppArmor user-namespace rule for the Bubblewrap executable. Native Windows confinement is not ready; use WSL2 for now.

Commands start with a stripped-down environment, Git metadata stays protected, common secret paths are blocked, and normal workspace mode has no network access. [SECURITY.md](SECURITY.md) documents the exact boundaries and known gaps. Read it before using `--preset yolo` or running GrokForge on code you do not trust.

## Still being built

- Rich diff rendering and native-scrollback polish
- The repository map and smarter automatic context selection
- Foreground auto-commit and a practical undo workflow
- Full session search
- Native Windows enforcement
- Signed installers and package-manager releases

## Work on GrokForge

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the project rules and [docs/design](docs/design) for the design record.

## License

[MIT](LICENSE)

Grok is a trademark of xAI. GrokForge is an independent project and is not affiliated with or endorsed by xAI.
