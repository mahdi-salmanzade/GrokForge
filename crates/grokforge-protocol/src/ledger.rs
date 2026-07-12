//! The context ledger's record type: one entry per source of bytes in an outbound request.
//! The sum of `bytes` across a request's entries must reconcile with the serialized body
//! size the client actually sent (ADR 0003).

use serde::{Deserialize, Serialize};

/// One accounted source of bytes leaving the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Where the bytes came from: a file path, `"repo_map"`, `"agents_md"`, `"conversation"`,
    /// `"system_prompt"`, `"tool_specs"`, or a tool name.
    pub source: String,
    /// Post-redaction byte count contributed to the request body.
    pub bytes: usize,
    /// How many secrets were redacted from this source.
    pub redactions: usize,
    /// Why it was included (`"mention"`, `"tool read"`, `"auto-context"`, ...).
    pub reason: String,
}

impl LedgerEntry {
    #[must_use]
    pub fn new(source: impl Into<String>, bytes: usize, reason: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            bytes,
            redactions: 0,
            reason: reason.into(),
        }
    }

    #[must_use]
    pub fn with_redactions(mut self, n: usize) -> Self {
        self.redactions = n;
        self
    }
}

/// A whole request's worth of ledger entries, with the reconciliation total.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestLedger {
    pub entries: Vec<LedgerEntry>,
}

impl RequestLedger {
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.entries.iter().map(|e| e.bytes).sum()
    }

    #[must_use]
    pub fn total_redactions(&self) -> usize {
        self.entries.iter().map(|e| e.redactions).sum()
    }

    pub fn push(&mut self, entry: LedgerEntry) {
        self.entries.push(entry);
    }
}
