# Local HTTP API (`grokforge serve`)

`grokforge serve` binds **loopback only** and speaks plain HTTP. It installs no CORS policy and
opens no public tunnel. For remote access, terminate TLS and authentication in a reverse proxy
that connects to the loopback listener; GrokForge itself refuses a non-loopback bind.

The default listen address is `127.0.0.1:4096`. Prefer `GROKFORGE_SERVER_TOKEN` — `--token` can
leak through shell history and process listings. The token must be 32–1024 bytes, with no
whitespace or control characters, and not an obvious low-entropy placeholder. If none is
supplied, GrokForge generates 32 random bytes, prints the encoded token once, and writes no token
to disk.

```sh
export GROKFORGE_SERVER_TOKEN="$(openssl rand -base64 32)"
grokforge serve --bind 127.0.0.1:4096
```

`/health` and `/openapi.json` are the only unauthenticated routes. They contain no project or
session data:

```sh
curl http://127.0.0.1:4096/health
curl http://127.0.0.1:4096/openapi.json
```

Session and prompt routes require `Authorization: Bearer …`:

```sh
curl -H "Authorization: Bearer $GROKFORGE_SERVER_TOKEN" \
  http://127.0.0.1:4096/v1/sessions

curl --no-buffer \
  -H "Authorization: Bearer $GROKFORGE_SERVER_TOKEN" \
  -H "Content-Type: application/json" \
  --data '{"prompt":"Explain the failing tests","plan":true}' \
  http://127.0.0.1:4096/v1/prompts
```

`POST /v1/prompts` returns `text/event-stream`. Each `data:` field is one serialized GrokForge
protocol event. The `x-grokforge-session-id` response header names the durable local session.
`GET /v1/sessions/{session_id}` also requires the bearer token.

The persistent server has no `yolo` preset. Project MCP still needs `--trust-project-mcp` plus an
exact `--allow mcp:<server>` grant, because there is no interactive approval screen:

```sh
grokforge serve --trust-project-mcp --allow mcp:docs
```

See [`crates/grokforge-server/README.md`](../../crates/grokforge-server/README.md) and
[SECURITY.md](../../SECURITY.md).
