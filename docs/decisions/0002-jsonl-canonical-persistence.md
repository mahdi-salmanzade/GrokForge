# ADR 0002 — JSONL rollouts are the canonical session record; SQLite is a derived index

- Status: accepted
- Date: 2026-07-12

## Context

Three design drafts disagreed on the canonical persistence format: SQLite as the
source of truth, append-only JSONL rollouts as the source of truth with SQLite as a
rebuildable index, or "size-capped rotating session storage" (which would silently
destroy resumable history). This must be settled before any session code is written.

## Decision

- The **canonical, authoritative record of a session is an append-only JSONL rollout
  file**, one per session, under the platform data dir (`directories` crate). Each line
  is one persisted `ResponseItem`/turn event. This is the best debugging artifact and
  test fixture, and it is append-only so it never loses history.
- **SQLite is a derived index** (list/search/resume metadata) that can be rebuilt from
  the JSONL rollouts if deleted. A test asserts rebuild-from-JSONL.
- **Size-caps apply to debug logs only** (the anti-"640 TB/yr" guard), never to session
  rollouts.

## Consequences

- `grokforge-core::store` writes JSONL first, then updates the index.
- Resume replays the JSONL up to the last coherent item boundary; an interrupted turn is
  marked and the user re-prompts (we do not resurrect in-flight SSE/tool state).
