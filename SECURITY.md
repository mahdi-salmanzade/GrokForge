# Security & Privacy

GrokForge is a coding agent: it reads your code, can edit files, and can run commands. This
document states plainly what it does with your data, what it protects against, and where the
limits are.

## Privacy claim

The built-in model client sends requests only to the API endpoint you configure with
`XAI_BASE_URL` (default `https://api.x.ai`). GrokForge has no telemetry code path. You provide the
API key with `XAI_API_KEY`, stores an API key in the OS keychain, or obtains a subscription token
through the browser-based OAuth flow.

Each first-party model request body is assembled through the context-ledger path. The ledger
reconciles its source entries to the byte length of the exact serialized JSON body and records
redaction counts. This accounting does **not** include HTTP headers (including the API key), API
responses, server-side provider activity, or network traffic produced by shell commands and
external processes. Detailed ledger entries are currently emitted by `grokforge exec --json`;
the plain-text and TUI frontends do not yet provide a complete human-facing ledger view. Startup
model validation also performs `GET /v1/models` against the configured endpoint; it sends no
project context and has no request body to include in the context ledger.

Run `grokforge doctor` to see the configured model endpoint, the selected sandbox backend, and
whether OS enforcement is active.

Grok-hosted web search, X search, and code interpreter are disabled by default. Enabling one sends
the model request to xAI with that hosted capability available; provider-side retrieval or code
execution is outside GrokForge's local command sandbox and context ledger. Treat returned content
as untrusted model input and review resulting actions.

## What leaves your machine

Model context is redacted before the request body is serialized:

- **Pattern redaction is on by default.** Context sources such as user input, repository
  instructions, conversation history, and tool output are scanned for recognized private keys,
  cloud credentials, bearer tokens, and assignment-style secrets. Matches are replaced before
  they enter the request.
- **Default secret-path globs** block built-in file tools from reading paths such as `.env`,
  `*.pem`, `*.key`, and common credential files. The command sandbox also tries to enforce these
  rules: Seatbelt combines read/write profile rules with bounded physical-target discovery, while
  bubblewrap masks only existing matches found during a bounded workspace scan.
- **Project skill catalogs are automatic context.** For each safely discovered
  `.grokforge/skills/*/SKILL.md`, GrokForge sends the bounded skill name, relative path, and
  frontmatter description through the redaction and ledger path. The instruction body remains
  local unless Grok deliberately reads that file with `read_file`. A project slash-command
  template is sent as ordinary redacted user input only when you invoke that command.
- These controls are defense in depth, not a proof that secrets cannot be read or disclosed.
  Pattern matching can miss unusual formats, bubblewrap masking is partial, and the `yolo`
  preset removes the default secret globs. Explicitly trusted external processes are outside
  these controls.

## MCP and external-process trust

A project `.grokforge/mcp.json` file can name arbitrary executables, so GrokForge treats it as
executable code. The TUI, headless, and resume frontends do **not** start project MCP servers by
default. Pass `--trust-project-mcp` on that invocation only after reviewing the project config and
the commands it names. An explicitly trusted MCP server runs as a separate process; its filesystem
access, network activity, and logs are outside the model-request ledger. Tool output is accounted
if it is later included in a model request, but that does not account for the external process's
own activity or egress. Treat MCP output as untrusted model input.

## Command sandboxing

Commands the agent runs are confined by the OS, not by the honor system:

| Platform | Current enforcement |
|---|---|
| macOS | Seatbelt (`sandbox-exec`) after a self-test: workspace-confined writes, Git metadata protected, private temporary storage, configured secret-glob read/write denial with bounded physical-target checks, and network denial by default. Wrapped profiles also deny Mach-service lookup, Apple Events, and signals to processes outside the same sandbox. This is a policy sandbox rather than a process namespace; process visibility and inherited channels remain separate boundaries. |
| Linux | A validated system bubblewrap (`bwrap` ≥ 0.11.2, non-setuid, and passing a namespace self-test): read-only root, writable workspace, Git metadata protected, common host sockets hidden, child capabilities dropped, and network namespace isolated by default. Before granting workspace writes, GrokForge requires every protected metadata path to exist and validates it with a bounded scan; unsafe aliases and non-regular entries fail closed. It separately scans up to 100,000 entries in every confined workspace and rejects hard-linked files, socket/FIFO/device entries, and directory symlinks that leave the scanned roots, including in read-only mode. Existing secret-glob matches are masked by another bounded scan, so secret-file enforcement is partial. |
| Windows and other unsupported hosts | No native enforcing command backend. Commands whose policy requires confinement and host-side file tools are refused. Run inside WSL2 with a working `bwrap` installation for Linux enforcement. Trusted host-Git operations are also unavailable on native Windows. |

The agent's host-side `write_file` and `edit` tools separately check workspace and protected-path
boundaries. On Unix they use descriptor-relative, no-follow operations and reject symlink and
hard-link targets. Native Windows and other non-Unix hosts do not currently have an equivalent
race-resistant implementation, so GrokForge refuses all host-side file tools there. Use WSL2.

`grokforge doctor` reports whether enforcement is active. If a backend is unavailable, the
fallback reports `enforced = false` and fails closed for normal policies instead of running a
command while ignoring requested confinement.

Sandboxed child processes and their OS wrappers start from an empty environment. They receive
only a validated executable search path, narrowly validated standard locale/terminal settings,
and backend-created private temporary-directory settings. Host home/config paths, proxies,
credential helpers, CI/cloud variables, and other ambient values are not inherited. Commands
that require a host credential or configuration variable must therefore be configured through a
separate, explicit future trust flow; there is no ambient-environment exception today.

Landlock and seccomp are planned Linux improvements in the design roadmap; they are not shipped
in the current implementation. Proxy-routed/domain-allowlisted networking is also not
implemented: a non-full network policy is isolated rather than silently broadened.

## Git auto-commit and undo

Git mutations run from the trusted host process, never inside the command sandbox. Foreground
sessions never auto-commit: even staging only recorded file-tool paths cannot distinguish a user
or sibling process that races a write to the same path. Their changes remain in the working tree
for review. A subagent owns a mode-0700 worktree in GrokForge's per-user data directory, outside
all parent project workspaces, and its command sandbox writes only within that worktree. It may
therefore stage its descriptor-safe direct file-tool paths and create a session-tagged commit
after verifying that worktree began clean, but only when the turn used no shell, MCP, or custom
tool. Those tools can leave descendants outside process-group lifetime containment on macOS, so
any such turn is preserved uncommitted for manual review.
Shell-created files are never swept into an automatic commit. Commit creation can still fail—for
example because identity is not configured—in which case the isolated worktree remains
uncommitted.

This trusted-host boundary is currently Unix-only. GrokForge accepts Git only from validated
system locations, starts it with a minimal environment, and disables repository-configured hooks,
filters, merge drivers, and text-conversion commands. Windows ACL/owner validation is not yet
implemented, so native Windows fails closed: auto-commit, `/undo`, and subagent worktree creation
are unavailable. These features work under WSL2 subject to the normal Linux requirements.

`/undo` operates only on successful commits attributed to the current session. It is not a
general rollback for uncommitted edits, failed commits, concurrent user changes, or commits from
another session. Review `git status` and the resulting diff before relying on it.

## Threat model & known limits

- **Prompt injection.** Repository content, command output, provider-supplied search results, and
  any future MCP tool output are untrusted input to an agent that can edit files and request
  commands. The mitigations are the sandbox and approval workflow; review proposed actions. Do
  not run untrusted repositories in `yolo`.
- **`yolo` is intentionally dangerous.** It removes approvals, workspace-only filesystem
  confinement, network isolation, and the default secret-path blocks. Protected Git metadata
  remains deny-write for both commands and host-side file tools. Because that protection still
  requires enforcement, a `yolo` shell fails closed when no supported sandbox backend is
  available. Use `yolo` only in a disposable environment.
- **Full-access commands can make network connections** and can send data that the context ledger
  does not observe. Approving a normally sandboxed command does not itself remove its network
  isolation; selecting the full-access/`yolo` mode does.
- **Unsupported sandbox hosts fail closed for shell policies, but that is not full platform
  isolation.** GrokForge itself and its host-side tools still run on the host.
- **Linux Git-metadata protection deliberately rejects some repositories.** Workspace-write and
  normal `yolo` shells require an ordinary `.git` entry at the workspace root and safely
  pinnable protected metadata. Workspaces whose protected Git metadata is missing, exceeds the
  scan limit, contains symlinks or other non-regular entries, or contains hard-linked files
  cannot run those shell policies. Read-only shell policy is not subject to this writable-path
  precondition.
- **Confined-tree checks trade compatibility for isolation.** On both enforcing backends, a
  workspace containing a hard-linked file, socket, FIFO, device node, more than 100,000 scanned
  entries, or a directory symlink whose target leaves the approved roots is refused—even for a
  read-only command. These checks happen before launch and cannot eliminate filesystem races by
  another same-user process.
- **Seatbelt is not a process container.** Its current profile restricts filesystem writes,
  configured secret reads and writes, network operations, new Mach-service lookup, Apple Events,
  and cross-sandbox signals. It does not hide process visibility or revoke descriptors and Mach
  rights inherited before profile application.
- **Redaction is best-effort pattern matching.** It can miss unusual secret formats, transformed
  values, encoded data, or secrets obtained by an external process.

## Reporting a vulnerability

Please do not open a public issue for security vulnerabilities. Report privately to the
maintainers (contact address to be published with the first tagged release). We aim to acknowledge
within a few days.
