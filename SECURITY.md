# Security & Privacy

GrokForge is a coding agent: it reads your code, can edit files, and can run commands. This
document states plainly what it does with your data, what it protects against, and where the
limits are.

## Privacy claim

**GrokForge makes no network connections except to endpoints you explicitly configure, and every
request to the model is accounted for in the context ledger.**

- The only first-party network destination is the API endpoint you set (`XAI_BASE_URL`, default
  `https://api.x.ai`). You bring your own API key.
- MCP servers you connect are **external processes**; their own network activity is outside our
  audit scope and is flagged as such in the ledger/`/mcp`.
- **Telemetry is off. There is no telemetry code path.** There is nothing to opt into.

Run `grokforge doctor` to see the configured endpoint, the active sandbox backend, and whether it
is actually enforced.

## What leaves your machine

Only what is needed to answer the model, and only after redaction:

- **Secret redaction is on by default.** File contents, tool output, and your own typed/pasted
  input are scanned for private keys, cloud credentials, bearer tokens, and `KEY=secret`-style
  assignments; matches are replaced with `[REDACTED:<rule>]` before anything is sent.
- **Secret files are never read.** `.env`, `*.pem`, `*.key`, `id_rsa`, and similar are blocked;
  the model is told the file is withheld.
- The **context ledger** accounts, byte-for-byte, for every source included in each request.

## Command sandboxing

Commands the agent runs are confined by the OS, not by the honor system:

| Platform | Enforcement |
|---|---|
| macOS | Seatbelt (`sandbox-exec`): writes confined to the workspace, `.git` read-only, network denied in workspace-write mode |
| Linux | bubblewrap (`bwrap`) when present: read-only root, workspace bind-mounted, network namespace unshared |
| Windows | Approval-only. Run inside WSL2 for the Linux sandbox. |

The agent's own file writes (`write_file`/`edit`) additionally enforce the workspace boundary in
the host process, so they cannot escape it even though they don't shell out.

`grokforge doctor` reports whether enforcement is actually active on your machine. If a backend is
unavailable, GrokForge falls back to approval-only and **says so** — it never claims protection it
isn't providing.

## Threat model & known limits

- **Prompt injection.** Content returned by `web_search`/`x_search` and MCP tools is untrusted
  input to an agent that can edit files and run commands. The mitigations are the sandbox and the
  approval workflow — review what the agent proposes. Do not run untrusted repositories in `yolo`.
- **`yolo` preset disables all protection** (no sandbox, no approvals). Use only in throwaway
  environments.
- Linux without `bwrap`, and Windows without WSL2, are **approval-only** — no OS confinement.
- Redaction is best-effort pattern matching; it can miss unusual secret formats. The blocked-file
  and sandbox layers are the other lines of defense.

## Reporting a vulnerability

Please do not open a public issue for security vulnerabilities. Report privately to the
maintainers (contact address to be published with the first tagged release). We aim to acknowledge
within a few days.
