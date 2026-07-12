# ADR 0004 — The privacy claim is exactly what the egress test asserts

- Status: accepted
- Date: 2026-07-12

## Context

The strongest marketing wedge (versus Grok Build's full-repo-upload scandal) is a
verifiable privacy claim. A claim stronger than the test is a liability that will be
found and amplified. Built-in `web_fetch`, connected MCP servers, and an optional xAI
Docs MCP all egress from the process, so "the only network call is to the API endpoint"
would be false.

## Decision

- **Claim wording:** "No network egress except endpoints you explicitly configure — and
  every byte is visible in the ledger." Not "only the API endpoint."
- **Test (M11 gate, re-run every release):** a full-session run behind an egress-recording
  proxy asserts (a) every contacted host is in the configured allowlist (API base URL +
  enabled MCP/tool endpoints), and (b) every request to the API has a ledger entry.
- **Zero telemetry:** no telemetry code paths exist; CI audits for their absence. First-run
  shows a static privacy statement — there is no telemetry opt-in prompt to build.
- **`web_fetch` is cut from v0.1** to keep the egress surface minimal (server-side
  `web_search` covers the need).
- MCP servers are external processes; the ledger panel flags "egress not audited" for them.

## Consequences

- The claim and the test move together. Changing one requires changing the other.
