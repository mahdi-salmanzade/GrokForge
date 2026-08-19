# Examples

Short, copy-pasteable walkthroughs for unreleased GrokForge surfaces. They assume a local
`grokforge` binary (`cargo build --release` from the workspace root). Telemetry: GrokForge has
none.

| Example | What it covers |
|---|---|
| [custom-tools](custom-tools/) | Hash-pinned local executables |
| [serve](serve/) | Loopback HTTP API |
| [repo-map](repo-map/) | Bounded lexical workspace map |
| [code-intelligence](code-intelligence/) | One-shot local LSP and formatters |

Design notes live under [`docs/`](../docs/). These examples are the short path to the same
behavior.

## Trust flags

Project files never auto-run. Review the file, then pass the matching flag on that TUI, `exec`,
`resume`, `acp`, or `serve` invocation (`doctor` accepts only `--trust-project-config`):

| Flag | What it trusts |
|---|---|
| `--trust-project-tools` | `.grokforge/tools.toml` (hash-pinned custom executables) |
| `--trust-project-mcp` | `.grokforge/mcp.json` (local stdio or remote MCP servers) |
| `--trust-project-config` | `.grokforge/config.toml` (billable model and runtime settings only) |

Owner files under `~/.grokforge/` load without these flags after Unix ownership, permission, and
link checks succeed. Trusting a project declaration still does not auto-approve model-selected MCP
or custom-tool calls. Non-interactive `exec` and `serve` need an exact `--allow mcp:<server>` for
MCP; `serve` has no `yolo` preset.

See [SECURITY.md](../SECURITY.md).

## Hidden debug commands

`grokforge debug` is hidden from top-level `--help`. The live subcommands are:

```sh
grokforge debug api "say hi"
grokforge debug sandbox -- true
grokforge debug repomap --budget 2000
```

`debug sandbox` runs the command under the default workspace-write policy and reports whether the
OS backend actually enforced it. `debug repomap` is local-only.
