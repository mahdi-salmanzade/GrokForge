//! `grokforge-xai` — GrokForge's in-house client for the xAI Grok API.
//!
//! Grok-only by design (there is no provider trait in v1); this crate is the isolated seam
//! where a multi-provider abstraction could later be introduced. It targets the Responses
//! API (`POST /v1/responses`) and streams typed events over SSE.
//!
//! Design rules enforced here:
//! - **Base URL and model IDs are data, never constants** (the xAI→SpaceXAI reorg will move
//!   the domain; retired slugs silently redirect).
//! - **The initial request is retried with backoff; mid-stream failures are surfaced**, since
//!   the Responses API has no resume token — the agent loop owns whole-request replay.
//! - **Every request's serialized size is exposed** ([`ResponseStream::request_bytes`]) so the
//!   context ledger can reconcile byte-for-byte with what actually left the machine.

mod client;
mod error;
mod event;
mod model;
pub mod oauth;
mod request;
mod stream;

pub use client::{RequestAttempt, RetryConfig, XaiClient};
pub use error::XaiError;
pub use event::{EncryptedReasoning, StopReason, StreamEvent, ToolCall, Usage};
pub use model::ModelInfo;
pub use oauth::{OAuthError, OAuthTokens};
pub use request::{
    ContentPart, Effort, FunctionTool, InputItem, Reasoning, ResponsesRequest, Role, ServerTool,
    ToolDef, model_supports_effort,
};
pub use stream::ResponseStream;
