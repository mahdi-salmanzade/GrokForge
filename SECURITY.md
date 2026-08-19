# Security & Privacy

GrokForge is a coding agent: it reads your code, can edit files, and can run commands. This
document states plainly what it does with your data, what it protects against, and where the
limits are.

## Privacy claim

The built-in model client sends requests only to the API endpoint you configure with
`XAI_BASE_URL` (default `https://api.x.ai`). GrokForge has no telemetry code path. You provide the
API key with `XAI_API_KEY`, or store an API key / subscription OAuth tokens in a password-encrypted
file at `~/.grokforge/credentials.enc`. OAuth tokens for configured remote MCP servers are stored
in that same encrypted payload. Argon2id derives the key from your password plus a random salt;
ChaCha20-Poly1305 seals the payload, and the file has `0600` permissions on Unix. GrokForge does
**not** use the OS keychain or any system secret store.

Each first-party model request body is assembled through the context-ledger path. The ledger
reconciles its source entries to the byte length of the exact serialized JSON body and records
redaction counts. This accounting does **not** include HTTP headers (including the API key), API
responses, server-side provider activity, or network traffic produced by shell commands and
external processes. Detailed ledger entries stream in `grokforge exec --json`. Plain-text `exec` prints a compact
source/byte/redaction summary on stderr and can write an owner-private JSON export with
`--ledger <path>` (exclusive create; it will not overwrite an existing file). In the TUI, `/ledger`
shows session totals and a bounded newest-source window, and `/status` includes the same totals.
Neither view is a complete alt-screen audit panel, and neither includes HTTP headers, API
responses, local stdio MCP process egress, or shell-command traffic. Startup model validation also
performs `GET /v1/models` against the configured endpoint; it sends no project context and has no
request body to include in the context ledger.

Run `grokforge doctor` to see the configured model endpoint, the selected sandbox backend, and
whether OS enforcement is active.

Grok-hosted web search, X search, and code interpreter are disabled by default. Enabling one sends
the model request to xAI with that hosted capability available; provider-side retrieval or code
execution is outside GrokForge's local command sandbox and context ledger. Treat returned content
as untrusted model input and review resulting actions.

## What leaves your machine

Subscription sign-in opens an xAI authorization URL at `https://auth.x.ai` in your browser and
exchanges the returned authorization code—or a stored refresh token—with xAI's token endpoint.
Those OAuth requests contain authentication material but no repository or conversation context;
the loopback callback listens only on `127.0.0.1`, validates an unguessable state value, and the
token client does not follow redirects.

Remote MCP OAuth setup first contacts the configured MCP endpoint for an authorization challenge,
then fetches protected-resource and authorization-server metadata and contacts the discovered token
endpoint. The browser visits the discovered authorization endpoint. These requests carry OAuth
protocol data but no repository or conversation context. GrokForge's discovery and token clients
use bounded responses, ignore ambient proxies, and do not follow redirects; the external browser
has its own redirect and proxy behavior. This setup/control traffic happens before a model session
and is not represented as remote JSON-RPC egress in the context ledger.

Model context is redacted before the request body is serialized:

- **Pattern redaction is on by default.** Context sources such as user input, repository
  instructions, conversation history, and tool output are scanned for recognized private keys,
  cloud credentials, bearer tokens, and assignment-style secrets. Matches are replaced before
  they enter the request.
- **Default secret-path globs** block built-in file tools from reading paths such as `.env`,
  `*.pem`, `*.key`, and common credential files. The command sandbox also tries to enforce these
  rules: Seatbelt combines read/write profile rules with bounded physical-target discovery, while
  bubblewrap masks only existing matches found during a bounded workspace scan.
- **GrokForge's encrypted credential file is a permanent sandbox exception.** The native macOS and
  Linux backends deny or mask both the default file and `GROKFORGE_CREDENTIALS_PATH`, even when
  full-access mode disables the broader ambient-secret catalogue.
- **Project skill catalogs are automatic context.** For each safely discovered
  `.grokforge/skills/*/SKILL.md`, GrokForge sends the bounded skill name, relative path, and
  frontmatter description through the redaction and ledger path. The instruction body remains
  local unless Grok deliberately reads that file with `read_file`. A project slash-command
  template is sent as ordinary redacted user input only when you invoke that command.
- These controls are defense in depth, not a proof that secrets cannot be read or disclosed.
  Pattern matching can miss unusual formats, bubblewrap masking is partial, and the `yolo`
  preset removes the default secret globs. Explicitly trusted external processes are outside
  these controls.

## Session persistence and image attachments

Session rollouts are owner-private persistence, not encrypted storage. On Unix, GrokForge creates
the session directory with mode `0700` and rollout files with mode `0600`, rejects unsafe link
aliases, and appends bounded JSONL records. Those records contain model-visible conversation and
tool history. Anyone who can read files as your user can read them.

PNG and JPEG attachments are copied into the rollout as ordinary base64 text alongside their MIME
type. Base64 is an encoding, not encryption. Text redaction cannot inspect secrets visible in image
pixels, and the image bytes are sent to the configured model endpoint as image input. The local
caps are 4 MiB per image, 8 MiB total, and eight images per prompt. JSON session exports preserve
the encoded image data as well; treat rollouts and exports as sensitive files and delete them when
they are no longer needed.

## MCP and external-process trust

A project `.grokforge/mcp.json` file can name arbitrary executables, remote endpoints, explicit
credential-bearing headers, and OAuth authorization metadata, so GrokForge treats it as executable
configuration. No agent frontend starts it by default. Pass `--trust-project-mcp` on that TUI,
headless, resume, ACP, or local-API invocation only after reviewing the complete project config.

Local stdio servers run as separate processes; their filesystem access, network activity, and logs
remain outside GrokForge's request ledger. Remote Streamable HTTP endpoints require HTTPS except
for loopback, do not follow redirects, and receive only configured headers. Header values may
expand explicitly named `${NAME}` environment variables; values are held as sensitive headers and
are not included in diagnostics. Every remote JSON-RPC request emits a ledger entry containing only
its server/method label and exact serialized-body byte count—never its URL, headers, arguments, or
body. Tool output is accounted again if later included in a model request, but neither mechanism
audits what the remote service or a local MCP process does after receiving data. Treat all MCP
output as untrusted model input.

Remote OAuth is an alternative to a configured `Authorization` header; the two cannot be combined
for one server. `grokforge login --mcp <name>` is an explicit decision to trust that selected
project declaration. GrokForge supports protected-resource metadata discovery (including the
`WWW-Authenticate` link and well-known fallback), authorization-server discovery, Authorization
Code with PKCE S256 and an unguessable state, RFC 8707 resource indicators, and the exact configured
loopback callback URI. Authorization and token endpoints require HTTPS except for loopback
development. Every MCP HTTP request receives the Bearer token, and redirects are disabled.

MCP access and refresh tokens are stored only in `credentials.enc`, bound to the MCP endpoint,
resource, issuer, client ID, and redirect URI. Before using an unexpired token GrokForge checks the
locally verifiable binding; refresh performs discovery again, requires the complete stored binding
to match, preserves rotated refresh tokens, and re-seals the record with the same password. A
client secret, when required, may only be named via `client_secret_env`; it is not embedded in the
project file or credential record. Dynamic Client Registration and Client ID Metadata Documents
are not implemented, so the client ID and exact loopback redirect must be pre-registered. Runtime
`insufficient_scope` step-up is not automatic; update the configured scopes and sign in again.
Password-encrypted MCP tokens also require a terminal in which GrokForge can ask for the credential
password. Protocol-stdin and other non-interactive frontends cannot unlock them; use an explicitly
configured environment-backed header instead in those environments.

Trusting the declaration and authorizing a tool call are separate decisions. The TUI prompts for
each unapproved MCP boundary. Non-interactive frontends deny calls unless the exact server is
pre-granted with `--allow mcp:<server>` (or the headless-only `yolo` preset is selected); the local
HTTP server does not support `yolo`.

## Custom executable-tool trust

`~/.grokforge/tools.toml` is an owner trust root and is loaded automatically only after Unix
ownership, permissions, and link checks succeed. A project `.grokforge/tools.toml` is ignored
unless that frontend is started with `--trust-project-tools`. Both manifests can cause model-
selected native code to run, so review every executable, fixed argument, schema, filesystem mode,
network declaration, and explicitly allowed environment name before installing or trusting one.

Each declaration pins a SHA-256 digest. On every call GrokForge opens the source without following
symlinks, copies and hashes it into an owner-private temporary executable, and executes that staged
copy with an exact argv rather than a shell. This closes path-replacement attacks between hash
verification and execution; a matching digest does **not** make the program trustworthy. Calls use
the normal approval, OS sandbox, timeout, cancellation, bounded JSON stdin/stdout, and result-
redaction paths. Non-mutating declarations receive a read-only policy, and network declarations
still require the applicable approval. Explicit environment allowlists reject credential-like and
runtime-injection names. Any filesystem or network activity that an approved executable can perform
is external-process activity and is not fully described by the model-request ledger.

Owner-manifest ACL validation is currently Unix-only and fails closed on other platforms. See
[`docs/custom-tools-v1.md`](docs/custom-tools-v1.md) for the complete manifest rules and limits.

## Local language servers and formatters

Language servers and formatters are locally installed native programs, not parsers embedded in
GrokForge. Built-in commands are resolved from the owner's executable search path; overrides come
only from the owner-private code-intelligence configuration. GrokForge reverifies the executable
immediately before each bounded, one-shot spawn, passes exact argv without a shell, disables
networking, and prevents the language server from writing. Formatter processes can write only a
private scratch copy; GrokForge performs the final descriptor-bound workspace replacement itself.

GrokForge proactively sends only the requested document over LSP, but initialization also names
the workspace root. The language server can inspect the workspace and other read-only filesystem
paths visible inside the OS sandbox. Common credential stores and configured secret paths are
masked, returned locations are restricted to non-secret workspace files, and tool output is
redacted, but a hostile installed server could still encode unrelated readable data into an
otherwise valid hover or diagnostic result. Treat the configured executable as trusted local code.
Formatter conflict checks narrow concurrent-overwrite races but have the same non-CAS limitation
described under command sandboxing below.

## Authenticated local HTTP API

`grokforge serve` binds only to a loopback IP and speaks plain HTTP. Every session or prompt route
requires `Authorization: Bearer ...`; only `/health` and the static `/openapi.json` are public. The
token must be bounded and nontrivial. If none is supplied, GrokForge generates 32 random bytes,
prints the encoded token once, and writes no token to disk. Router state retains only its SHA-256
digest and compares presented digests in constant time; in-memory CLI secret owners are redacted
from `Debug` output and zeroized on drop. Prefer `GROKFORGE_SERVER_TOKEN` because `--token` can leak
through shell history and process listings.

The API installs no permissive CORS policy and bounds request bodies, running prompt tasks, event
count/size, and turn duration. Dropping or exhausting an event stream requests cooperative turn
cancellation; the client does not synchronously wait, but the background task retains its admission
permit and workspace lock until already-running host mutations finish safely and the turn exits.
The network-facing Agent event queue is hard-bounded. Trusted project setup warnings and remote-MCP
byte-accounting events still pass through a legacy unbounded callback bridge, but their producers
are separately capped by project-server, pagination, request, tool-call, and turn-iteration limits;
streamed model deltas and normal tool activity do not use that bridge.
Execute prompts take a process-local workspace write lock, so only one mutating API turn runs at a
time. Plan prompts may run together under read locks, but never overlap an execute turn or observe
one half-applied. This single-writer rule does not coordinate external editors, shells, or another
GrokForge process.
API sessions disable Git auto-commit.
The persistent API also has no `yolo` preset; its approval and sandbox capabilities are fixed when
the process starts.

Loopback is not encryption or protection from another process already running as your user. Keep
the bearer secret. For remote access, terminate TLS and authentication in a reverse proxy that
connects to the loopback listener; GrokForge itself will still refuse a non-loopback bind.

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

Successful host writes use an owner-directory temporary file, `fsync`, and an atomic rename.
`edit`, patch, and formatter writeback also compare the physical identity and expected contents
immediately before replacement; `write_file` checks physical identity but has no expected-content
precondition because it is explicitly an overwrite operation. These checks detect many concurrent
replacements, but they are not a portable cross-process content compare-and-swap and do not lock
out another same-user writer. An
uncooperative process can still modify the same inode in the final check-to-rename window. This is
why GrokForge does not claim ownership of foreground changes or offer foreground undo.

`grokforge doctor` reports whether enforcement is active. If a backend is unavailable, the
fallback reports `enforced = false` and fails closed for normal policies instead of running a
command while ignoring requested confinement.

Sandboxed child processes and their OS wrappers start from an empty environment. They receive
only a validated executable search path, narrowly validated standard locale/terminal settings,
and backend-created private temporary-directory settings. Host home/config paths, proxies,
credential helpers, CI/cloud variables, and other ambient values are not inherited. Commands
run by the ordinary `shell` tool have no ambient-environment exception. Custom executable tools
have a separate explicit environment allowlist that rejects secret-like and runtime-injection
names; MCP HTTP headers and OAuth client secrets likewise resolve only variables explicitly named
in trusted MCP configuration.

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

Foreground `/undo` and `/redo` are deliberately disabled. The dormant foreground journal is not a
security boundary: GrokForge cannot yet prove that a same-path editor save or other process write
belongs to the agent, and restoring a snapshot could destroy user work. Foreground edits therefore
stay uncommitted and must be reviewed or reverted with normal Git/file tools.

The internal isolated-worktree undo primitive can operate only on successful commits attributed to
the current session. It is not a general rollback for uncommitted edits, failed commits, concurrent
changes, or commits from another session. The ordinary TUI does not currently expose a parent
action for those child commits and hides `/undo`; isolated-worktree redo is not available.

## Threat model & known limits

- **Prompt injection.** Repository content, command output, provider-supplied search results, and
  MCP or custom-tool output are untrusted input to an agent that can edit files and request
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
- **Image pixels are opaque to text redaction and rollout encryption does not exist.** Attached
  images are sent to the model and retained as plaintext base64 in owner-private JSONL until the
  session/export is deleted.
- **File mutation is conflict detection, not a universal CAS.** Descriptor-relative operations,
  identity/content rechecks, and atomic rename prevent many path and partial-write attacks, but
  cannot exclude an uncooperative same-user writer in the last check-to-rename window.

## Reporting a vulnerability

Please do not open a public issue for security vulnerabilities. Report privately to the
maintainers (contact address to be published with the first tagged release). We aim to acknowledge
within a few days.
