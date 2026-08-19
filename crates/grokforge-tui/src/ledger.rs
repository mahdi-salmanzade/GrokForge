//! Bounded in-memory copy of context-ledger entries for the interactive session.
//!
//! Session totals live on [`crate::app::App`] and keep counting after this ring drops oldest
//! rows, so `/status` and the compact status-line ↑files/bytes remain lifetime numbers.

use std::collections::VecDeque;

use grokforge_protocol::LedgerEntry;

/// Oldest entries fall off once a session exceeds this many `LedgerAppended` events.
pub(crate) const MAX_LEDGER_ENTRIES: usize = 128;
/// `/ledger` prints this many newest retained rows so the transcript stays scannable.
pub(crate) const LEDGER_VIEW_ENTRIES: usize = 24;

/// Honest scope line required by the UX spec and SECURITY.md.
pub(crate) const LEDGER_SCOPE_NOTE: &str = "This is the model-request (and remote MCP JSON-RPC) ledger. Headers, API responses, stdio MCP process egress, and shell traffic are outside it.";

/// Ring of the most recent ledger rows observed this session.
#[derive(Debug, Default)]
pub(crate) struct SessionLedger {
    entries: VecDeque<LedgerEntry>,
}

impl SessionLedger {
    pub(crate) fn push(&mut self, entry: LedgerEntry) {
        if self.entries.len() >= MAX_LEDGER_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Chronological window of the newest `limit` retained entries.
    pub(crate) fn recent(&self, limit: usize) -> impl Iterator<Item = &LedgerEntry> {
        let skip = self.entries.len().saturating_sub(limit);
        self.entries.iter().skip(skip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_oldest_entries_after_the_session_cap() {
        let mut ledger = SessionLedger::default();
        for index in 0..130 {
            ledger.push(LedgerEntry::new(
                format!("src/{index}.rs"),
                index,
                "tool read",
            ));
        }
        assert_eq!(ledger.len(), MAX_LEDGER_ENTRIES);
        let sources: Vec<&str> = ledger
            .recent(MAX_LEDGER_ENTRIES)
            .map(|entry| entry.source.as_str())
            .collect();
        assert_eq!(sources.first().copied(), Some("src/2.rs"));
        assert_eq!(sources.last().copied(), Some("src/129.rs"));
        assert_eq!(sources.len(), 128);
    }
}
