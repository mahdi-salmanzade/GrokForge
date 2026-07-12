# ADR 0005 — ACP (editor embedding) is deferred; the protocol crate is the hedge

- Status: accepted
- Date: 2026-07-12

## Context

The tech brief lists ACP (Agent Client Protocol, for editor embedding à la Zed) among
the table stakes Grok Build already meets. The user-locked scope is "full differentiator
v0.1," but v0.1 is already very large, and three independent design drafts silently
deferred ACP. That deferral needs to be explicit rather than accidental.

## Decision

- **ACP is out of scope for v0.1.**
- The hedge is `grokforge-protocol`: all core↔frontend communication is a serde-serializable
  `Op`/`Event` channel pair with zero TUI types in the core. The headless `--json` frontend
  already proves the seam works. An ACP adapter is a later, additive frontend crate over the
  same protocol — no core rearchitecting required.

## Consequences

- Keep `grokforge-protocol` frontend-agnostic and append-only after release.
- Revisit ACP after v0.1 ships and stabilizes.
