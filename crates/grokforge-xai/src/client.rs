//! The `XaiClient` — the one place GrokForge talks to the network.
//!
//! Grok-only by design (no provider trait); this crate is the future multi-provider seam.
//! Base URL and auth are data, never constants. The initial request is retried with
//! exponential backoff on transport/429/5xx failures — but only *before* any events reach
//! the caller, which keeps replay idempotent. Mid-stream failures are surfaced, not
//! silently retried; the agent loop owns whole-request replay.

use std::time::Duration;

use url::Url;

use crate::error::XaiError;
use crate::model::{ModelInfo, ModelsResponse};
use crate::request::ResponsesRequest;
use crate::stream::ResponseStream;

/// Retry policy for establishing the initial response.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(20),
        }
    }
}

/// A configured client for one xAI endpoint.
#[derive(Debug, Clone)]
pub struct XaiClient {
    http: reqwest::Client,
    base_url: Url,
    api_key: String,
    retry: RetryConfig,
}

impl XaiClient {
    /// Build a client. `base_url` is the API root (e.g. `https://api.x.ai`).
    pub fn new(base_url: &str, api_key: impl Into<String>) -> Result<Self, XaiError> {
        let base_url = Url::parse(base_url).map_err(|e| XaiError::InvalidBaseUrl(e.to_string()))?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("grokforge/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(XaiError::Transport)?;
        Ok(Self {
            http,
            base_url,
            api_key: api_key.into(),
            retry: RetryConfig::default(),
        })
    }

    #[must_use]
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    fn endpoint(&self, path: &str) -> Result<Url, XaiError> {
        self.base_url
            .join(path)
            .map_err(|e| XaiError::InvalidBaseUrl(e.to_string()))
    }

    /// List models advertised by the endpoint.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, XaiError> {
        let url = self.endpoint("/v1/models")?;
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(XaiError::Transport)?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        let body = resp.bytes().await.map_err(XaiError::Transport)?;
        let parsed: ModelsResponse = serde_json::from_slice(&body)?;
        Ok(parsed.data)
    }

    /// Confirm a model slug is advertised by the endpoint. Returns [`XaiError::UnknownModel`]
    /// (with the available list) when it isn't — the retired-slug remap hook.
    pub async fn validate_model(&self, model: &str) -> Result<(), XaiError> {
        let available = self.list_models().await?;
        if available.iter().any(|m| m.id == model) {
            Ok(())
        } else {
            Err(XaiError::UnknownModel {
                model: model.to_string(),
                available: available.into_iter().map(|m| m.id).collect(),
            })
        }
    }

    /// Serialize a request and return the body plus its exact byte length. Exposed so the
    /// context ledger can reconcile its per-source byte accounting against what is actually sent.
    pub fn serialize_request(req: &ResponsesRequest) -> Result<(Vec<u8>, usize), XaiError> {
        let body = serde_json::to_vec(req)?;
        let len = body.len();
        Ok((body, len))
    }

    /// Open a streaming response. Retries the initial request on transport/429/5xx errors
    /// with exponential backoff before returning the stream.
    pub async fn stream(&self, req: &ResponsesRequest) -> Result<ResponseStream, XaiError> {
        let url = self.endpoint("/v1/responses")?;
        let (body, request_bytes) = Self::serialize_request(req)?;

        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let send_result = self
                .http
                .post(url.clone())
                .bearer_auth(&self.api_key)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .body(body.clone())
                .send()
                .await;

            match send_result {
                Ok(resp) if resp.status().is_success() => {
                    return Ok(ResponseStream::from_response(resp, request_bytes));
                }
                Ok(resp) => {
                    let err = error_from_response(resp).await;
                    if err.is_retriable() && attempt < self.retry.max_attempts {
                        self.backoff(attempt, err.retry_after()).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(e) => {
                    if attempt < self.retry.max_attempts {
                        self.backoff(attempt, None).await;
                        continue;
                    }
                    return Err(XaiError::Transport(e));
                }
            }
        }
    }

    async fn backoff(&self, attempt: u32, server_hint: Option<Duration>) {
        let delay = server_hint.unwrap_or_else(|| {
            let exp = self
                .retry
                .base_delay
                .saturating_mul(1u32 << (attempt - 1).min(16));
            exp.min(self.retry.max_delay)
        });
        tracing::debug!(attempt, ?delay, "retrying xAI request after backoff");
        tokio::time::sleep(delay).await;
    }
}

/// Classify a non-success HTTP response into the error taxonomy, reading the body for a
/// message and the `Retry-After` header for 429s.
async fn error_from_response(resp: reqwest::Response) -> XaiError {
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = resp.text().await.unwrap_or_default();
    let message = extract_message(&body);

    match status {
        401 | 403 => XaiError::Auth { status, message },
        429 => XaiError::RateLimited {
            status,
            retry_after,
            message,
        },
        500..=599 => XaiError::Server { status, message },
        _ => XaiError::Api { status, message },
    }
}

fn extract_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(String::from)
                .or_else(|| v.get("message").and_then(|m| m.as_str()).map(String::from))
        })
        .unwrap_or_else(|| {
            if body.is_empty() {
                "no response body".to_string()
            } else {
                body.chars().take(200).collect()
            }
        })
}
