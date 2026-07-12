//! `grokforge-test-support` — shared test infrastructure.
//!
//! - [`mock`]: a byte-controllable HTTP/1.1 mock of the xAI API for exercising the SSE
//!   client and reconciling request byte counts against the context ledger.
//!
//! Fixture repositories and a PTY harness are added alongside the milestones that need them.

// This crate is test-only scaffolding: panicking on a poisoned lock or malformed fixture is
// the desired behavior, so the workspace-wide no-unwrap/expect rule does not apply here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

pub mod mock;

pub use mock::{MockXai, MockXaiBuilder, Received, Reply};
