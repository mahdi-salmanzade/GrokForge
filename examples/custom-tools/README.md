# Custom executable tools

GrokForge custom tools are small programs, not in-process plugins. Each call uses exact argv (no
shell), a one-request/one-response JSON protocol, the active OS sandbox, the normal approval flow,
and secret redaction.

This directory has a complete v1 example:

- [`line_count.py`](line_count.py) — count lines in one UTF-8 workspace file (Python stdlib only)
- [`tools.toml.example`](tools.toml.example) — the matching manifest

The full contract and limits are in [`docs/custom-tools-v1.md`](../../docs/custom-tools-v1.md).

## Hash pin and executable rules

`command` must be an **absolute** path to a regular executable owned by the current user or root
and not group/world-writable. The manifest pins its SHA-256. GrokForge checks that digest on load
and again on every call by copying the bytes through an already-open no-follow descriptor into a
new owner-private temporary executable, then running that verified copy.

```sh
chmod 700 examples/custom-tools/line_count.py
shasum -a 256 examples/custom-tools/line_count.py
```

Put the 64-hex digest in `sha256` and the absolute path in `command`. A matching digest does not
make the program trustworthy — review the source, schema, `mutating` / `network` flags, and
`env_allowlist` before installing.

Owner-manifest ACL and executable ownership checks are Unix-only. Other platforms fail closed for
both owner and trusted project custom tools.

## Owner manifest (loads automatically)

```sh
mkdir -p ~/.grokforge
chmod 700 ~/.grokforge
cp examples/custom-tools/tools.toml.example ~/.grokforge/tools.toml
# edit command + sha256, then:
chmod 600 ~/.grokforge/tools.toml
grokforge exec -p "count the lines in examples/custom-tools/line_count.py"
```

On Unix the file is accepted only when `~/.grokforge` is owner-only (`0700` or stricter) and
`tools.toml` is a regular, singly linked, owner-only (`0600` or stricter) file owned by you.

## Project manifest (ignored unless trusted)

A repo may ship `.grokforge/tools.toml`. GrokForge ignores it unless that frontend is started with
`--trust-project-tools` after you review the manifest and every pinned executable:

```sh
grokforge --trust-project-tools
grokforge exec --trust-project-tools -p "count the lines in src/lib.rs"
```

Plan turns do not load custom tools. Names cannot replace a built-in, custom, or MCP tool already
registered.
