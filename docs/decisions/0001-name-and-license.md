# ADR 0001 — Name, trademark posture, and license

- Status: accepted
- Date: 2026-07-12

## Context

The project is a third-party open-source client for xAI's Grok. "Grok" is an
xAI trademark. The name "grok-cli" is already taken by superagent-ai. We need a
name that is available and a simple permissive license.

## Decision

- **Name:** GrokForge; binary `grokforge`.
- **Trademark posture:** treat "Grok" as xAI's trademark. Use it only nominatively
  ("a client for Grok"), never implying endorsement. Add a trademark disclaimer to
  the README before public launch. **Action item (pre-launch):** review xAI brand
  guidelines and, if needed, rename to a non-"Grok" mark. This is tracked and not
  yet cleared.
- **License:** MIT. It is short, permissive, and familiar to contributors and users.

## Consequences

- Contributions and bundled code must be compatible with MIT. This is documented in
  `CONTRIBUTING.md` and checked on the dependency side by `cargo deny`.
- The name carries residual trademark risk until the pre-launch review is done.
