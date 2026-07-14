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

    /// 401 — the API key is missing or invalid (the credential itself failed).
    #[error("authentication failed ({status}): {message}")]
    Auth { status: u16, message: String },

    /// 402/403 — the key authenticated, but the account/team lacks access: no credits, no
    /// license, or the model/endpoint isn't permitted. Distinct from [`Auth`] because the fix
    /// is billing/permissions, not the key.
    #[error("access denied ({status}): {message}")]
    AccessDenied { status: u16, message: String },

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

    /// A configured bearer credential could not be found (environment or encrypted file) — the
    /// caller should prompt when interactive.
    #[error("no xAI API key configured")]
    NoApiKey,

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

    /// Whether this denial is a billing/credits/license problem (the key is fine, the account
    /// can't pay), as opposed to a bad credential.
    #[must_use]
    pub fn is_billing(&self) -> bool {
        match self {
            XaiError::AccessDenied { message, .. } | XaiError::RateLimited { message, .. } => {
                let m = message.to_lowercase();
                m.contains("credit")
                    || m.contains("license")
                    || m.contains("purchase")
                    || m.contains("billing")
                    || m.contains("spending limit")
            }
            _ => false,
        }
    }

    /// The first `https://` URL mentioned in the provider message (e.g. the billing console link).
    #[must_use]
    pub fn console_url(&self) -> Option<String> {
        let (XaiError::AccessDenied { message, .. }
        | XaiError::Auth { message, .. }
        | XaiError::Api { message, .. }
        | XaiError::RateLimited { message, .. }) = self
        else {
            return None;
        };
        message
            .split_whitespace()
            .find(|t| t.starts_with("https://"))
            .map(|s| {
                s.trim_end_matches(['.', ',', '"', ')', '\'', '}', ']', '>', ';'])
                    .to_string()
            })
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

    #[test]
    fn access_denied_is_billing_and_extracts_url() {
        let error = XaiError::AccessDenied {
            status: 403,
            message:
                "Your team doesn't have any credits yet. Purchase at https://console.x.ai/team/abc."
                    .into(),
        };
        assert!(error.is_billing(), "credits message should be billing");
        assert_eq!(
            error.console_url().as_deref(),
            Some("https://console.x.ai/team/abc")
        );
        // And it must NOT display as an authentication failure.
        assert!(!error.to_string().contains("authentication failed"));
        assert!(error.to_string().contains("access denied"));
    }

    #[test]
    fn plain_auth_failure_is_not_billing() {
        let error = XaiError::Auth {
            status: 401,
            message: "invalid api key".into(),
        };
        assert!(!error.is_billing());
    }
}
