# Local LSP and formatters

GrokForge does **not** vendor, download, or install language servers. Built-in commands are
resolved from the owner's executable `PATH` (`rust-analyzer`, `typescript-language-server`,
`pyright-langserver`, `gopls`, `clangd`, plus matching formatters). Missing executables fail at
spawn.

Overrides belong in the **owner-private** `~/.grokforge/code-intelligence.toml`. They are not
read from `.grokforge/config.toml`, and `--trust-project-config` does not load language servers.

Each `lsp_diagnostics` / `lsp_query` call launches **one** server in the read-only OS sandbox with
network disabled, sends a bounded initialize → didOpen → one query → shutdown/exit lifecycle, and
reaps the process. There is no long-lived language-server session and no retained index between
calls. `format_file` runs the formatter against a **private copy**; GrokForge performs the final
descriptor-bound workspace replacement itself.

Details and current gaps: [`docs/lsp-code-intelligence.md`](../../docs/lsp-code-intelligence.md).

## Owner config

On Unix the file is accepted only from a private `~/.grokforge` directory, as a regular, singly
linked, owner-only (`0600`) file. Owner-configured `command` values must be **absolute** paths
(no `PATH` lookup, no `{placeholders}` in the executable name). Formatter `args` must contain
`{file}` so the private copy can be substituted. `{root}`, `{workspace}`, and `{file_relative}`
are also expanded as exact argv values, never through a shell.

```sh
mkdir -p ~/.grokforge
chmod 700 ~/.grokforge
```

```toml
# ~/.grokforge/code-intelligence.toml
# Default true: owner entries are searched first, then built-in PATH tools.
extend_defaults = true

[[language]]
name = "rust-override"
extensions = ["rs"]
language_id = "rust"
root_markers = ["Cargo.toml"]

[language.lsp]
command = "/absolute/path/to/rust-analyzer"
timeout_ms = 15000
diagnostic_wait_ms = 2000

[language.formatter]
command = "/absolute/path/to/rustfmt"
args = ["--edition", "2024", "--config-path", "{root}", "{file}"]
timeout_ms = 15000
```

```sh
chmod 600 ~/.grokforge/code-intelligence.toml
```

Set `extend_defaults = false` only when the file fully replaces the built-in language table.

## Try it

Install the relevant server yourself, then:

```sh
grokforge exec -p "run lsp_diagnostics on crates/grokforge/src/main.rs"
grokforge exec -p "use lsp_query hover on the Cli struct in crates/grokforge/src/main.rs"
```

`lsp_query` operations: `hover`, `definition`, `references`, `document_symbols`,
`workspace_symbols`, `implementation`. Position-based calls use one-based `line` and one-based
UTF-16 `character`. Completion, rename, code actions, semantic tokens, and call hierarchy are
deliberately not implemented yet.
