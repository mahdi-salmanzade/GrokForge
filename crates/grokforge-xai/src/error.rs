//! Error taxonomy for the xAI client.
//!
//! Callers branch on these to decide retry (transport/429/5xx), remap (retired model
//! slug → open the `/model` picker), or hard-fail (auth). `is_retriable` and `retry_after`
//! centralize the retry policy so the client and the agent loop agree.

use std::time::Duration;

/// Everything that can go wrong talking to the xAI API.
#[derive(Debug, thiserror::Error)]
pub enum XaiError {
    /// The configured base URL could not be parsed or joined.
    #[error("invalid base URL: {0}")]
    InvalidBaseUrl(String),

    /// Transport-level failure (DNS, connect, TLS, socket reset before/at send).
    #[error("request transport error: {0}")]
    Transport(#[source] reqwest::Error),

    /// 401/403 — the API key is missing, invalid, or lacks access.
    #[error("authentication failed ({status}): {message}")]
    Auth { status: u16, message: String },

    /// 429 — rate limited. `retry_after` carries the server's hint when present.
    #[error("rate limited ({status}); retry after {retry_after:?}: {message}")]
    RateLimited {
        status: u16,
        retry_after: Option<Duration>,
        message: String,
    },

    /// 5xx — server-side error, generally worth retrying.
    #[error("server error ({status}): {message}")]
    Server { status: u16, message: String },

    /// Other non-success status (4xx that isn't auth/429), including a retired model slug.
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },

    /// A configured model slug is not present in `GET /v1/models` — likely retired and
    /// silently redirected (and re-priced). The caller should prompt to remap.
    #[error("model `{model}` is not available on this endpoint")]
    UnknownModel {
        model: String,
        available: Vec<String>,
    },

    /// The SSE body failed mid-stream (connection dropped, malformed frame). The whole
    /// request must be replayed by the caller; there is no resume token.
    #[error("stream error: {0}")]
    Stream(String),

    /// A response body could not be deserialized.
    #[error("decode error: {0}")]
    Decode(#[source] serde_json::Error),

    /// The fully serialized request would exceed the local egress/memory safety limit.
    #[error("request body exceeds the {max}-byte safety limit")]
    RequestTooLarge { max: usize },

    /// The API reported an error inside the stream (`error` / `response.failed` event).
    #[error("model reported error: {0}")]
    ApiStreamError(String),
}

impl XaiError {
    /// Whether replaying the request stands a chance of succeeding.
    #[must_use]
    pub fn is_retriable(&self) -> bool {
        matches!(
            self,
            XaiError::Transport(_)
                | XaiError::RateLimited { .. }
                | XaiError::Server { .. }
                | XaiError::Stream(_)
        )
    }

    /// The server-suggested delay before retrying, if any.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            XaiError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl From<serde_json::Error> for XaiError {
    fn from(e: serde_json::Error) -> Self {
        XaiError::Decode(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_display_keeps_provider_message() {
        let error = XaiError::RateLimited {
            status: 429,
            retry_after: Some(Duration::from_secs(2)),
            message: "monthly spending limit reached".into(),
        };
        assert!(error.to_string().contains("monthly spending limit reached"));
    }
}
