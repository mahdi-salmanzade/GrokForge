# Repository map

`repo_map` gives the model a compact, local-only view of a workspace without running project
code. It inventories source-oriented text files and extracts declaration names (not function
bodies) for common languages, including Rust, Python, JavaScript/TypeScript, Go, Java/Kotlin,
C/C++, C#, Swift, Ruby, PHP, shell, Lua, Elixir, and Scala.

## Safety and privacy

- The operation is read-only and has no network code path.
- Project `.gitignore` and `.ignore` files are respected. Machine-global ignore files and ignore
  files above the workspace are deliberately not consulted, so output is deterministic across
  machines.
- Hidden files, binary files, unsupported file types, and version-control metadata are omitted.
- Active `secrets.deny` globs are applied before a candidate file is opened.
- Traversal stays on the repository root's filesystem, and symbolic links are never followed. On
  Unix, every component is opened relative to the already
  opened repository root with `O_NOFOLLOW`; multiply-linked files are rejected. Other platforms
  reject symlink/reparse components and require the final canonical path to remain below the root.
- Cancellation is checked before walking, between entries, and between file reads.

## Fixed limits

The default map considers at most 20,000 filesystem entries, descends at most 16 levels, inspects
at most 1,000 candidate files, reads at most 128 KiB per file and 2 MiB total, retains at most 48
symbols per file, and returns at most 48 KiB. Internal hard ceilings prevent any caller from
raising those values above 50,000 entries, 32 levels, 2,000 files, 512 KiB per file, 8 MiB total,
128 symbols per file, or 128 KiB of output. The model-callable tool exposes only `max_bytes`, capped
at 64 KiB, and an optional 512-byte local relevance query.

Files and declarations are ranked deterministically by query match, entry-point/manifest value,
language, symbol match, path depth, and lexical path. A partial-read or truncation marker is
included when a bound is reached.
