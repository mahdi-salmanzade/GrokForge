# Custom executable tools (v1)

GrokForge custom tools are small, language-neutral programs. They are **not** dynamically loaded
libraries and never run inside the GrokForge process. Every call uses an exact argv, a bounded JSON
stdin/stdout protocol, the active OS sandbox, the normal approval flow, cancellation, and the
normal secret-redaction path.

The owner manifest is `~/.grokforge/tools.toml`. On Unix it is accepted only when
`~/.grokforge` is owned by the current user and inaccessible to group/other users, and the file is
owned by the current user, has one hard link, and is inaccessible to group/other users:

```sh
mkdir -p ~/.grokforge
chmod 700 ~/.grokforge
chmod 600 ~/.grokforge/tools.toml
```

Owner-manifest ACL and executable ownership/mode validation are currently implemented on Unix.
Other platforms fail closed for both owner and explicitly trusted project custom tools rather than
pretending those trust roots were verified; use WSL2 on Windows.

A project may provide `.grokforge/tools.toml`, but GrokForge ignores it unless that project-tool
manifest is explicitly trusted by the frontend. Trust is never inferred from the repository.

## Manifest

```toml
version = 1

[[tool]]
name = "line_count"
description = "Count lines in one UTF-8 workspace file."
command = "/absolute/path/to/line_count.py"
sha256 = "REPLACE_WITH_64_HEX_CHARACTERS"
args = []
input_schema = { type = "object", properties = { path = { type = "string" } }, required = ["path"], additionalProperties = false }
mutating = false
parallel_safe = true
network = false
env_allowlist = []
timeout_ms = 5000
max_output_bytes = 16384
```

Calculate the digest after installing the executable:

```sh
chmod 700 /absolute/path/to/line_count.py
shasum -a 256 /absolute/path/to/line_count.py
```

`command` must be an absolute, regular executable owned by the current user or root and not
group/world-writable. GrokForge checks its SHA-256 when loading and again for every call. For the
call, it copies the bytes through an already-open no-follow descriptor into a new owner-private
temporary executable while hashing, then runs that verified copy. Replacing the source path after
verification cannot replace the program that is about to execute.

The manifest declares the static arguments exactly. GrokForge never invokes a shell. The child
starts from an empty ambient environment plus GrokForge's small usability baseline (`PATH`, valid
locale/terminal variables, and `NO_COLOR`). `env_allowlist` may explicitly copy additional
non-secret host variables. Credential-like names, provider variables, loader/runtime injection
variables, Git variables, and baseline overrides are rejected.

`mutating = false` is enforced with a read-only filesystem policy, not treated as documentation.
`network = false` forces network isolation even if the surrounding session is broader. A
network-enabled tool still requires approval for its exact executable first. Under an isolated
session it can receive network only after the sandbox classifies a real network denial and the
user separately approves that retry. Project/workspace writes and `.git` protection remain under
the normal sandbox policy.

## JSON protocol

GrokForge writes exactly one JSON value to stdin and closes it:

```json
{
  "protocol_version": 1,
  "call_id": "tool-call-id",
  "workspace_root": "/absolute/workspace",
  "arguments": { "path": "src/main.rs" }
}
```

The executable must exit with status 0 and write exactly one response object to stdout. Logs go
to stderr (and still count against the output budget).

Success:

```json
{"protocol_version":1,"ok":true,"content":"123 lines"}
```

Tool-level failure:

```json
{"protocol_version":1,"ok":false,"error":"path is not a UTF-8 file"}
```

Unknown response fields, malformed JSON, a nonzero exit, timeout, schema mismatch, integrity
failure, sandbox denial, cancellation, or excess output becomes a failed tool result. Tool result
text passes through GrokForge's secret redactor before it can enter model context.

## Hard limits

| Boundary | v1 limit |
|---|---:|
| Manifest | 256 KiB |
| Tools per manifest | 32 |
| Total registered tools (built-ins + custom + MCP) | 128 |
| Tool name | 64 bytes |
| Description | 1,024 bytes |
| JSON Schema | 32 KiB, depth 32, 2,048 nodes |
| Schema references | disabled |
| Static argv | 32 entries, 4 KiB each, 16 KiB total |
| Explicit environment names | 16 |
| JSON input envelope | 256 KiB |
| Executable | 64 MiB |
| Runtime | 120 seconds maximum |
| Captured stdout + stderr | 64 KiB maximum; manifest may lower it |

Names cannot replace built-in, custom, or MCP tools already registered. When the 128-tool provider
limit is reached, later custom tools are reported and skipped rather than displacing built-ins.

## Authoring in any language

There is no GrokForge library dependency. Read one JSON object from stdin, check
`protocol_version`, validate any semantic constraints beyond the advertised JSON Schema, and emit
one response object. See [`examples/custom-tools/line_count.py`](../examples/custom-tools/line_count.py)
and [`examples/custom-tools/tools.toml.example`](../examples/custom-tools/tools.toml.example).

Current v1 intentionally has no package installer, dependency resolver, in-process ABI, secret
broker, streaming output, or persistent daemon protocol. Use MCP when a tool needs a long-lived or
remote service; use this protocol for small, reviewable local executables.
