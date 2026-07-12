# Contributing to GrokForge

Thanks for helping build a trustworthy open-source coding agent. A few rules keep the project clean and legally sound.

## Licensing of contributions

By submitting a contribution you agree it is dual-licensed under **MIT OR Apache-2.0**, the same as the project, unless you state otherwise in writing.

## The study-don't-copy rule (important)

Several excellent agents are Apache-2.0 (notably OpenAI's `codex-rs`). You may **read them for patterns and ideas**, but you may **not paste their code** into GrokForge. GrokForge is offered under `MIT OR Apache-2.0`; code that is Apache-2.0-only cannot be redistributed under MIT. Reimplement from your own understanding. This includes non-obvious artifacts like sandbox policy files (SBPL). Uncopyrightable facts — e.g. *which* syscalls to block — are fine to reproduce; the code that does the blocking is not.

PRs found to contain pasted third-party code will be closed. `cargo deny check` enforces the dependency side (no GPL/Fair-Source deps).

## Before you open a PR

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

All four must pass. TUI changes need reviewed insta snapshots (`cargo insta review`, commit the `.snap` files).

## Style

- No `unwrap`/`expect` outside tests.
- Libraries return `Result` with `thiserror` types; binaries use `color-eyre`.
- Keep `grokforge-protocol` additive after a release.
- Match the surrounding code; small, focused PRs.

## Reporting security issues

Do not open public issues for vulnerabilities — see `SECURITY.md` (published with v0.1) for private disclosure.
