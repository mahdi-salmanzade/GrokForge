# ADR 0001 — Name, trademark posture, and license

- Status: accepted
- Date: 2026-07-12

## Context

The project is a third-party open-source client for xAI's Grok. "Grok" is an
xAI trademark. The name "grok-cli" is already taken by superagent-ai. We need a
name that is available and a license that maximizes adoption while allowing us to
study (but not copy) Apache-2.0 reference implementations.

## Decision

- **Name:** GrokForge; binary `grokforge`.
- **Trademark posture:** treat "Grok" as xAI's trademark. Use it only nominatively
  ("a client for Grok"), never implying endorsement. Add a trademark disclaimer to
  the README before public launch. **Action item (pre-launch):** review xAI brand
  guidelines and, if needed, rename to a non-"Grok" mark. This is tracked and not
  yet cleared.
- **License:** dual `MIT OR Apache-2.0` (Rust ecosystem convention). Chosen over
  Apache-2.0-only (the tech brief's suggestion) for maximum permissiveness and over
  Fair-Source/FSL (which deters contributors, per the competitor research).

## Consequences

- codex-rs and other Apache-2.0-only code is a **pattern reference only**; it cannot
  be pasted, because Apache-2.0-only code cannot be redistributed under MIT. Enforced
  by `CONTRIBUTING.md` and `cargo deny check`.
- The name carries residual trademark risk until the pre-launch review is done.
