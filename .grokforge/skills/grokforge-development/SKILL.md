---
name: grokforge-development
description: Build, review, and verify GrokForge changes while preserving its privacy, sandbox, protocol, and terminal-UX guarantees.
---

# GrokForge development

Use this skill for changes to the GrokForge Rust workspace.

## Working order

1. Read the root `AGENTS.md` and the relevant design or decision record.
2. Trace the complete behavior through protocol, core, and frontend boundaries before editing.
3. Keep network-bound context on the ledgered request path. Never construct an unaccounted API request.
4. Keep Git mutations in the trusted host process and `.git` read-only inside every sandbox.
5. Make the smallest coherent change, then add a regression test at the narrowest useful layer.
6. Run formatting, the focused crate tests, workspace tests, and clippy with warnings denied.

## Terminal experience

- Prefer calm hierarchy and compact, meaningful chrome over large empty boxes.
- Render tool activity as human-readable actions; keep raw JSON available only as detail.
- Preserve useful information on narrow terminals and degrade decoration before content.
- Treat reasoning, approvals, retries, sandbox state, token usage, and ledger bytes as first-class state.
- Sanitize all provider, tool, path, Git, and model text before terminal rendering.

## Safety checks

- No `unwrap` or `expect` outside tests.
- Bound untrusted input, output, iteration counts, and filesystem walks.
- Refuse symlink and hard-link tricks in automatic context and host-process file operations.
- Default metered or networked capabilities off unless the user explicitly enables them.
- Preserve append-only protocol compatibility once a type has shipped.

## Verification

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
