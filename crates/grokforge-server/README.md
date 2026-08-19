# GrokForge local API

`grokforge serve` exposes a small, authenticated API on loopback. It does not add
CORS headers, public tunnels, telemetry, or a browser-facing control plane.

By default GrokForge generates an ephemeral bearer token and prints it once when
the server starts. Set `GROKFORGE_SERVER_TOKEN` to a private random value of at
least 32 characters when a stable token is needed. The shipped CLI refuses every
non-loopback bind; put a TLS reverse proxy in front of the loopback listener when
remote access is required. Prefer the environment variable because `--token` can
leak through shell history and process listings.

```sh
export GROKFORGE_SERVER_TOKEN="$(openssl rand -base64 32)"
grokforge serve --bind 127.0.0.1:4096

curl --no-buffer \
  -H "Authorization: Bearer $GROKFORGE_SERVER_TOKEN" \
  -H "Content-Type: application/json" \
  --data '{"prompt":"Explain the failing tests","plan":true}' \
  http://127.0.0.1:4096/v1/prompts
```

The prompt response is `text/event-stream`. Each `data:` field is one serialized
GrokForge protocol event. The `x-grokforge-session-id` response header identifies
the durable local session created for the request. The network-facing Agent queue
that carries model deltas and tool activity is hard-bounded; if a client cannot
drain it before it fills, GrokForge returns a terminal queue-limit event and
cooperatively cancels the turn instead of growing a model-output backlog. A legacy
callback bridge still carries bounded project-setup warnings and one small byte-
accounting event per capped remote-MCP request; it does not carry model deltas.

`--max-concurrency` bounds running prompt tasks, not just connected HTTP bodies. A
disconnect requests cancellation, while the admission slot and workspace lock stay
held until cooperative cleanup completes. Execute turns are single-writer; concurrent
plan turns use read locks and cannot overlap an execute turn.

Project MCP configuration still requires `--trust-project-mcp`. Because this server
has no interactive approval screen and deliberately has no `yolo` preset, explicitly
grant each callable server as well, for example:

```sh
grokforge serve --trust-project-mcp --allow mcp:docs
```

OpenAPI is available at `/openapi.json`; `/health` contains no private data and is
the only operational endpoint that does not require authentication.
