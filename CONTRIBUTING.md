# Contributing to GrokForge

Thanks for helping build a trustworthy open-source coding agent. A few rules keep the project clean and legally sound.

## Licensing of contributions

By submitting a contribution, you license it under the project's **MIT License**.

## Keep contributions original

Contributed source must be original, already available under MIT, or covered by written permission that allows it to be distributed under MIT. Learning from public ideas and interfaces is fine; copying their implementation is not.

Third-party libraries should remain dependencies with their own licenses and notices. Vendored or separately licensed source needs explicit maintainer review and must not be presented as MIT-only code. PRs containing unlicensed or incompatible third-party code will be closed. `cargo deny check` enforces the dependency side.

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
