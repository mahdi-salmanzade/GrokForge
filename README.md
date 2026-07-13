<div align="center">
  <img src="assets/grokforge.svg" alt="GrokForge" width="96" height="96" />
  <h1>GrokForge</h1>
  <p><strong>Make Grok great in the terminal.</strong></p>
</div>

GrokForge exists because using Grok from a terminal should feel like a serious development tool, not a chat window wired to `sh`.

Today it can read and edit project files, run commands, keep a session going, and hand isolated work to a subagent. When a command needs to cross a safety boundary, GrokForge stops and asks.

This is a pre-release project. The execution and safety paths are in place; rendering, automatic context selection, and distribution still need work.

## What works today

- Streaming conversations in a native Rust TUI
- Headless runs for scripts and CI
- File read, write, edit, list, glob, and grep tools
- Sandboxed shell commands with approval controls
- Read-only plan mode
- Persistent sessions with resume support
- Isolated subagent worktrees with scoped commits
- A context ledger that accounts for the request body sent to Grok
- Secret redaction, bounded output, and no telemetry

## Why another Grok CLI?

Because Grok deserves a CLI built for real repositories, not a thin wrapper around an API call.

Three constraints shape the implementation:

- Normal shell commands cannot write outside the workspace or use the network.
- Sandboxed commands cannot modify Git metadata.
- If the requested sandbox cannot be enforced, the command does not run.

## Build it

You need Rust 1.88 or newer.

```sh
git clone https://github.com/mahdi-salmanzade/GrokForge.git
cd GrokForge
cargo build --release
```

## Signing in

Just run it — GrokForge sets up credentials on first launch:

```sh
./target/release/grokforge
```

If you're not signed in yet, it asks how you want to connect:

- **[1] Your Grok subscription** (SuperGrok / X Premium+) — signs in through your browser (OAuth); usage bills against your subscription, no API key needed. *xAI currently limits subscription API access to the SuperGrok **Heavy** tier; other tiers may get a 403 until xAI lifts that.*
- **[2] An xAI API key** — paste a key from [console.x.ai](https://console.x.ai) (pay-as-you-go; new developer accounts also get free monthly credits via the data-sharing program).

Either choice is saved to your OS keychain, so it's a one-time step. You can also set things up ahead of time:

```sh
grokforge login                 # store an xAI API key in the OS keychain
grokforge login --subscription  # sign in with your SuperGrok / X Premium+ subscription
export XAI_API_KEY=your_key     # or just use an environment variable (best for CI)
```

Resolution order is env var → stored API key → subscription token → interactive prompt. Run `grokforge doctor` to see which credential is active and whether the sandbox is enforced.

Run one task without opening the TUI:

```sh
./target/release/grokforge exec -p "find the bug and explain the fix"
```

Useful commands:

```sh
grokforge doctor
grokforge sessions
grokforge resume
grokforge exec --plan -p "plan the refactor"
```

## Safety

macOS uses Seatbelt. Linux uses Bubblewrap 0.11.2 or newer; Ubuntu 24.04 also needs an AppArmor user-namespace rule for the Bubblewrap executable. Native Windows confinement is not ready; use WSL2 for now.

Commands start with a stripped-down environment, Git metadata stays protected, common secret paths are blocked, and normal workspace mode has no network access. [SECURITY.md](SECURITY.md) documents the exact boundaries and known gaps. Read it before using `--preset yolo` or running GrokForge on code you do not trust.

## Still being built

- Rich Markdown and diff rendering
- The repository map and smarter automatic context selection
- Foreground auto-commit and a practical undo workflow
- Full session search
- A user-facing trust flow for MCP servers
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
