# ADR 0003 — The context ledger is two layers with one reconciliation invariant

- Status: accepted
- Date: 2026-07-12

## Context

The context ledger is the product's trust wedge: it must account, byte-for-byte, for
everything sent to the API. Provenance ("this came from file X because it was
@-mentioned") is only knowable in the context assembler, but exact outbound byte counts
are only knowable at request serialization in the client. Putting the ledger in only one
place makes it either imprecise or provenance-blind.

## Decision

Two layers, one invariant:

1. **Provenance + redaction** live in `grokforge-core`'s `ContextAssembler`. It is the
   *only* code path that may construct an outbound request, enforced by type privacy: the
   client accepts a `LedgeredRequest` whose constructor is `pub(crate)` to core's assembler.
2. **Byte accounting** lives in `grokforge-xai` at serialization time.
3. **Invariant:** the sum of per-source bytes recorded by the assembler reconciles exactly
   with the serialized request body size measured by the client. A reconciliation test runs
   in the M2 mock-SSE suite from day one (not deferred to the ledger UI milestone).

## Consequences

- No raw request may reach the wire un-ledgered; this is an architectural rule, not a
  convention.
- Redaction runs over file reads, tool output, **and** user-typed/pasted composer input.
