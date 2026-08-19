# Repository map

`repo_map` is a **lexical, local, bounded** inventory of the workspace. It does not run project
code, follow symlinks, consult machine-global ignore files, or make a network request while
scanning. Declaration names (not function bodies) are extracted for common languages, including
Rust, Python, JavaScript/TypeScript, Go, Java/Kotlin, C/C++, C#, Swift, Ruby, PHP, shell, Lua,
Elixir, and Scala.

It is not a tree-sitter index, embedding store, or automatic semantic-context engine. Any text
later placed in a model request still goes through the normal redaction and context-ledger path.

Limits and ranking rules: [`docs/repo-map.md`](../../docs/repo-map.md).

## Model tool

The agent calls the built-in `repo_map` tool. Optional arguments:

| Argument | Meaning |
|---|---|
| `query` | Local ranking hint (names or concepts), at most 512 bytes. Never sent away. |
| `max_bytes` | UTF-8 cap on the returned map, 1024–65536 (default 49152). Other safety limits stay fixed. |

Ask GrokForge to use it:

```sh
grokforge exec -p "call repo_map with query billing, then summarize the matching crates"
```

Inside the TUI, `/tools` lists `repo_map` among the loaded local tools.

## `grokforge debug repomap`

Print the ranked map without starting a model turn. The `debug` parent is hidden from top-level
`--help`; this path is local-only and does not touch the network or the context ledger.

```sh
grokforge debug repomap
grokforge debug repomap --budget 2000
grokforge debug repomap --budget 2000 billing
```

`--budget` is the rendered-output cap in bytes (default 2000, clamped to the repository-map
limits). A positional query only changes local ranking.
