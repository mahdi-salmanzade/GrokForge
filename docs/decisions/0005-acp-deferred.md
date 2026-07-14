# ADR 0005 — ACP (editor embedding) is deferred; the protocol crate is the hedge

- Status: accepted (v0.1) — **superseded 2026-07-14: ACP v1 has since shipped**
- Date: 2026-07-12

## Update (2026-07-14)

The hedge paid off exactly as designed. `grokforge acp` now runs an ACP agent server over
newline-delimited JSON-RPC 2.0 on stdio as an **additive frontend** (`crates/grokforge/src/acp.rs`)
over the same `Op`/`Event` seam the headless frontend uses — no core changes were required.
Implemented for protocol version 1: `initialize`, `session/new`, `session/prompt` (streaming
`session/update` notifications + a `stopReason` response), `session/cancel`, and
`session/request_permission` bridged from the core approval engine (a custom `Approver` forwards
each request to the editor). Deferred within ACP: session persistence/`session/load`, client
`fs`/`terminal` calls, and image/audio prompt content. Credentials come from `XAI_API_KEY` because
stdin is the protocol channel. The original decision below is retained as the historical record.

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
