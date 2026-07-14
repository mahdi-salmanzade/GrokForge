//! The `XaiClient` — the one place GrokForge talks to the network.
//!
//! Grok-only by design (no provider trait); this crate is the future multi-provider seam.
//! Base URL and auth are data, never constants. The initial request is retried with
//! exponential backoff on transport/429/5xx failures — but only *before* any events reach
//! the caller, which keeps replay idempotent. Mid-stream failures are surfaced, not
//! silently retried; the agent loop owns whole-request replay.

use std::io::Write;
use std::time::Duration;

use url::Url;

use crate::error::XaiError;
use crate::model::{ModelInfo, ModelsResponse};
use crate::request::ResponsesRequest;
use crate::stream::ResponseStream;

#[derive(Debug, Clone, Copy)]
struct TimeoutConfig {
    connect: Duration,
    response_headers: Duration,
    read_idle: Duration,
    non_stream_body: Duration,
}

const DEFAULT_TIMEOUTS: TimeoutConfig = TimeoutConfig {
    connect: Duration::from_secs(15),
    response_headers: Duration::from_secs(120),
    read_idle: Duration::from_secs(300),
    non_stream_body: Duration::from_secs(30),
};
const MAX_MODELS_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

struct CappedBody {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl CappedBody {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            exceeded: false,
        }
    }
}

impl Write for CappedBody {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(next) = self.bytes.len().checked_add(buf.len()) else {
            self.exceeded = true;
            return Err(std::io::Error::other("request body size overflow"));
        };
        if next > MAX_REQUEST_BODY_BYTES {
            self.exceeded = true;
            return Err(std::io::Error::other("request body safety limit exceeded"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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

/// One outbound POST attempt, reported synchronously before the HTTP client sends it. Consumers
/// use this to account retries in their egress ledger even when all attempts ultimately fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestAttempt {
    pub number: u32,
    pub request_bytes: usize,
}

/// A configured client for one xAI endpoint.
#[derive(Clone)]
pub struct XaiClient {
    http: reqwest::Client,
    base_url: Url,
    api_key: String,
    retry: RetryConfig,
    response_header_timeout: Duration,
    non_stream_body_timeout: Duration,
}

impl std::fmt::Debug for XaiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XaiClient")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("retry", &self.retry)
            .field("response_header_timeout", &self.response_header_timeout)
            .field("non_stream_body_timeout", &self.non_stream_body_timeout)
            .finish_non_exhaustive()
    }
}

impl XaiClient {
    /// Build a client. `base_url` is the API root (e.g. `https://api.x.ai`).
    pub fn new(base_url: &str, api_key: impl Into<String>) -> Result<Self, XaiError> {
        Self::new_with_timeouts(base_url, api_key, DEFAULT_TIMEOUTS)
    }

    fn new_with_timeouts(
        base_url: &str,
        api_key: impl Into<String>,
        timeouts: TimeoutConfig,
    ) -> Result<Self, XaiError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(XaiError::Auth {
                status: 0,
                message: "API key cannot be empty".to_string(),
            });
        }
        let base_url = Url::parse(base_url).map_err(|e| XaiError::InvalidBaseUrl(e.to_string()))?;
        if !matches!(base_url.scheme(), "http" | "https")
            || base_url.cannot_be_a_base()
            || base_url.host_str().is_none()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(XaiError::InvalidBaseUrl(
                "expected an http(s) API root without credentials, query, or fragment".to_string(),
            ));
        }
        if base_url.scheme() == "http" && !is_loopback_url(&base_url) {
            return Err(XaiError::InvalidBaseUrl(
                "refusing to send an API key over plaintext HTTP to a non-loopback host"
                    .to_string(),
            ));
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("grokforge/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(timeouts.connect)
            // Resets after every successful read, so long-lived SSE streams remain valid while
            // connections that stop producing bytes are eventually released.
            .read_timeout(timeouts.read_idle)
            // Environment HTTP(S)_PROXY values are not part of GrokForge's configured egress
            // boundary. Proxy support needs an explicit, ledger-visible configuration first.
            .no_proxy()
            // The context ledger authorizes exactly one configured API origin. Following a 307
            // or 308 could replay the complete request body to an unconfigured host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(XaiError::Transport)?;
        Ok(Self {
            http,
            base_url,
            api_key,
            retry: RetryConfig::default(),
            response_header_timeout: timeouts.response_headers,
            non_stream_body_timeout: timeouts.non_stream_body,
        })
    }

    #[must_use]
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    fn endpoint(&self, path: &str) -> Result<Url, XaiError> {
        let mut url = self.base_url.clone();
        let base_ends_in_v1 = url
            .path_segments()
            .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
            == Some("v1");
        let mut requested = path
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty());
        let first = requested.next();
        let mut segments = url.path_segments_mut().map_err(|()| {
            XaiError::InvalidBaseUrl("base URL cannot contain path segments".to_string())
        })?;
        segments.pop_if_empty();
        if let Some(first) = first
            && !(base_ends_in_v1 && first == "v1")
        {
            segments.push(first);
        }
        for segment in requested {
            segments.push(segment);
        }
        drop(segments);
        Ok(url)
    }

    /// List models advertised by the endpoint.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, XaiError> {
        let url = self.endpoint("/v1/models")?;
        let send = self.http.get(url).bearer_auth(&self.api_key).send();
        let resp = tokio::time::timeout(self.response_header_timeout, send)
            .await
            .map_err(|_| {
                XaiError::Stream("timed out waiting for models response headers".to_string())
            })?
            .map_err(XaiError::Transport)?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp, self.non_stream_body_timeout).await);
        }
        let body = read_body_limited(
            resp,
            MAX_MODELS_RESPONSE_BYTES,
            "models",
            self.non_stream_body_timeout,
        )
        .await?;
        let parsed: ModelsResponse = serde_json::from_slice(&body)?;
        Ok(parsed.data)
    }

    /// Confirm a model slug is advertised by the endpoint. Returns [`XaiError::UnknownModel`]
    /// (with the available list) when it isn't — the retired-slug remap hook.
    pub async fn validate_model(&self, model: &str) -> Result<(), XaiError> {
        let available = self.list_models().await?;
        if available.iter().any(|candidate| {
            candidate.id == model || candidate.aliases.iter().any(|id| id == model)
        }) {
            Ok(())
        } else {
            Err(XaiError::UnknownModel {
                model: model.to_string(),
                available: available.into_iter().map(|m| m.id).collect(),
            })
        }
    }

    /// Best-effort lookup of a model's advertised context window (in tokens) from
    /// `GET /v1/models`. Returns `None` on any error, or when the endpoint does not advertise a
    /// window, so callers can fall back to a conservative default. Used to bound the assembled
    /// request so it stays under the model's hard prompt-length limit.
    pub async fn model_context_window(&self, model: &str) -> Option<u64> {
        let available = self.list_models().await.ok()?;
        available
            .into_iter()
            .find(|candidate| {
                candidate.id == model || candidate.aliases.iter().any(|id| id == model)
            })
            .and_then(|candidate| candidate.context_window)
    }

    /// Serialize a request and return the body plus its exact byte length. Exposed so the
    /// context ledger can reconcile its per-source byte accounting against what is actually sent.
    pub fn serialize_request(req: &ResponsesRequest) -> Result<(Vec<u8>, usize), XaiError> {
        let mut writer = CappedBody::new();
        if let Err(error) = serde_json::to_writer(&mut writer, req) {
            if writer.exceeded {
                return Err(XaiError::RequestTooLarge {
                    max: MAX_REQUEST_BODY_BYTES,
                });
            }
            return Err(error.into());
        }
        let body = writer.bytes;
        let len = body.len();
        Ok((body, len))
    }

    /// Open a streaming response. Retries the initial request on transport/429/5xx errors
    /// with exponential backoff before returning the stream.
    pub async fn stream(&self, req: &ResponsesRequest) -> Result<ResponseStream, XaiError> {
        self.stream_with_attempt_observer(req, |_| {}).await
    }

    /// Open a streaming response and synchronously report every POST attempt for egress
    /// accounting. The observer runs immediately before each send, including the final failed
    /// attempt when this method returns an error.
    pub async fn stream_with_attempt_observer<F>(
        &self,
        req: &ResponsesRequest,
        mut observe: F,
    ) -> Result<ResponseStream, XaiError>
    where
        F: FnMut(RequestAttempt),
    {
        let url = self.endpoint("/v1/responses")?;
        let (body, request_bytes) = Self::serialize_request(req)?;

        let mut attempt = 0u32;
        loop {
            attempt += 1;
            observe(RequestAttempt {
                number: attempt,
                request_bytes,
            });
            let send = self
                .http
                .post(url.clone())
                .bearer_auth(&self.api_key)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .body(body.clone())
                .send();
            let Ok(send_result) = tokio::time::timeout(self.response_header_timeout, send).await
            else {
                let error = XaiError::Stream(
                    "timed out waiting for streaming response headers".to_string(),
                );
                if attempt < self.retry.max_attempts {
                    self.backoff(attempt, None).await;
                    continue;
                }
                return Err(error);
            };

            match send_result {
                Ok(resp) if resp.status().is_success() => {
                    return Ok(ResponseStream::from_response(resp, request_bytes, attempt));
                }
                Ok(resp) => {
                    let err = error_from_response(resp, self.non_stream_body_timeout).await;
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
        let delay = self.retry_delay(attempt, server_hint);
        tracing::debug!(attempt, ?delay, "retrying xAI request after backoff");
        tokio::time::sleep(delay).await;
    }

    fn retry_delay(&self, attempt: u32, server_hint: Option<Duration>) -> Duration {
        server_hint
            .unwrap_or_else(|| {
                self.retry
                    .base_delay
                    .saturating_mul(1u32 << attempt.saturating_sub(1).min(16))
            })
            .min(self.retry.max_delay)
    }
}

/// Classify a non-success HTTP response into the error taxonomy, reading the body for a
/// message and the `Retry-After` header for 429s.
async fn error_from_response(resp: reqwest::Response, body_timeout: Duration) -> XaiError {
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let message = match read_body_prefix(resp, MAX_ERROR_RESPONSE_BYTES, body_timeout).await {
        Ok(body) => extract_message(&String::from_utf8_lossy(&body)),
        Err(error) => format!("could not finish reading error response body: {error}"),
    };

    match status {
        // 401 = bad/missing credential; 402/403 = key is fine but the account lacks
        // access (no credits/license) — a billing/permissions problem, not an auth one.
        401 => XaiError::Auth { status, message },
        402 | 403 => XaiError::AccessDenied { status, message },
        429 => XaiError::RateLimited {
            status,
            retry_after,
            message,
        },
        500..=599 => XaiError::Server { status, message },
        _ => XaiError::Api { status, message },
    }
}

async fn read_body_limited(
    mut response: reqwest::Response,
    limit: usize,
    label: &str,
    body_timeout: Duration,
) -> Result<Vec<u8>, XaiError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(XaiError::Stream(format!(
            "{label} response body exceeds {limit} byte limit"
        )));
    }

    tokio::time::timeout(body_timeout, async move {
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(XaiError::Transport)? {
            let Some(next_len) = body.len().checked_add(chunk.len()) else {
                return Err(XaiError::Stream(format!(
                    "{label} response body size overflow"
                )));
            };
            if next_len > limit {
                return Err(XaiError::Stream(format!(
                    "{label} response body exceeds {limit} byte limit"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    })
    .await
    .map_err(|_| {
        XaiError::Stream(format!(
            "timed out after {body_timeout:?} reading {label} response body"
        ))
    })?
}

async fn read_body_prefix(
    mut response: reqwest::Response,
    limit: usize,
    body_timeout: Duration,
) -> Result<Vec<u8>, XaiError> {
    tokio::time::timeout(body_timeout, async move {
        let mut body = Vec::new();
        while body.len() < limit {
            let Some(chunk) = response.chunk().await.map_err(XaiError::Transport)? else {
                break;
            };
            let remaining = limit - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        }
        Ok(body)
    })
    .await
    .map_err(|_| {
        XaiError::Stream(format!(
            "timed out after {body_timeout:?} reading response body"
        ))
    })?
}

fn extract_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(String::from)
                // xAI often returns `{"code": "...", "error": "message text"}` — a plain string.
                .or_else(|| v.get("error").and_then(|e| e.as_str()).map(String::from))
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

fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn stalled_server(send_sse_headers: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stalled server");
        let address = listener.local_addr().expect("stalled server address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut received = Vec::new();
            let mut chunk = [0u8; 1024];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.expect("read request");
                if read == 0 {
                    return;
                }
                received.extend_from_slice(&chunk[..read]);
            }
            if send_sse_headers {
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write response headers");
                socket.flush().await.expect("flush response headers");
            }
            std::future::pending::<()>().await;
        });
        format!("http://{address}")
    }

    async fn dribbling_error_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind dribbling server");
        let address = listener.local_addr().expect("dribbling server address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut received = Vec::new();
            let mut chunk = [0u8; 1024];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.expect("read request");
                if read == 0 {
                    return;
                }
                received.extend_from_slice(&chunk[..read]);
            }
            socket
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("write error headers");
            for _ in 0..20 {
                if socket.write_all(b"1\r\nx\r\n").await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        format!("http://{address}")
    }

    #[test]
    fn debug_never_exposes_api_key() {
        let client = XaiClient::new("https://api.x.ai", "super-secret-key").unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("super-secret-key"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn empty_or_whitespace_api_key_is_rejected() {
        assert!(matches!(
            XaiClient::new("https://api.x.ai", ""),
            Err(XaiError::Auth { status: 0, .. })
        ));
        assert!(matches!(
            XaiClient::new("https://api.x.ai", " \t\n"),
            Err(XaiError::Auth { status: 0, .. })
        ));
    }

    #[test]
    fn endpoint_preserves_proxy_prefix_and_accepts_v1_base() {
        let prefixed = XaiClient::new("https://gateway.test/xai-proxy/", "key").unwrap();
        assert_eq!(
            prefixed.endpoint("/v1/responses").unwrap().as_str(),
            "https://gateway.test/xai-proxy/v1/responses"
        );

        let versioned = XaiClient::new("https://gateway.test/xai-proxy/v1", "key").unwrap();
        assert_eq!(
            versioned.endpoint("/v1/models").unwrap().as_str(),
            "https://gateway.test/xai-proxy/v1/models"
        );
    }

    #[test]
    fn rejects_non_http_or_ambiguous_base_urls() {
        assert!(XaiClient::new("file:///tmp/api", "key").is_err());
        assert!(XaiClient::new("https://user:pass@example.test", "key").is_err());
        assert!(XaiClient::new("https://example.test?tenant=one", "key").is_err());
    }

    #[test]
    fn plaintext_http_is_limited_to_loopback_test_endpoints() {
        assert!(XaiClient::new("http://api.x.ai", "key").is_err());
        assert!(XaiClient::new("http://192.0.2.1:8080", "key").is_err());
        assert!(XaiClient::new("http://localhost:8080", "key").is_ok());
        assert!(XaiClient::new("http://127.0.0.1:8080", "key").is_ok());
        assert!(XaiClient::new("http://[::1]:8080", "key").is_ok());
    }

    #[test]
    fn server_retry_hint_is_capped_by_configured_maximum() {
        let client = XaiClient::new("https://api.x.ai", "key")
            .unwrap()
            .with_retry(RetryConfig {
                max_attempts: 3,
                base_delay: Duration::from_secs(1),
                max_delay: Duration::from_secs(5),
            });
        assert_eq!(
            client.retry_delay(1, Some(Duration::from_secs(3_600))),
            Duration::from_secs(5)
        );
    }

    fn short_test_timeouts() -> TimeoutConfig {
        TimeoutConfig {
            connect: Duration::from_secs(1),
            response_headers: Duration::from_millis(100),
            read_idle: Duration::from_millis(250),
            non_stream_body: Duration::from_millis(250),
        }
    }

    #[tokio::test]
    async fn response_header_wait_is_bounded_and_attempt_is_observed() {
        let base_url = stalled_server(false).await;
        let client = XaiClient::new_with_timeouts(&base_url, "key", short_test_timeouts())
            .expect("client")
            .with_retry(RetryConfig {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            });
        let request = ResponsesRequest::new("m", vec![]);
        let mut attempts = Vec::new();
        let started = std::time::Instant::now();
        let error = client
            .stream_with_attempt_observer(&request, |attempt| attempts.push(attempt))
            .await
            .expect_err("headers should time out");
        assert!(
            error.is_retriable(),
            "timeout should be retriable: {error:?}"
        );
        assert!(
            matches!(&error, XaiError::Stream(message) if message.contains("response headers")),
            "outer response-header deadline did not fire: {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "header wait was not bounded: {error:?}"
        );
        assert_eq!(
            attempts.len(),
            1,
            "timed-out attempt remains ledger-visible"
        );
    }

    #[tokio::test]
    async fn established_sse_stream_has_per_read_idle_timeout() {
        let base_url = stalled_server(true).await;
        let client = XaiClient::new_with_timeouts(&base_url, "key", short_test_timeouts())
            .expect("client")
            .with_retry(RetryConfig {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            });
        let request = ResponsesRequest::new("m", vec![]);
        let mut stream = client.stream(&request).await.expect("response headers");
        let error = stream
            .next()
            .await
            .expect("timeout event")
            .expect_err("idle stream should fail");
        assert!(matches!(error, XaiError::Stream(_)));
    }

    #[tokio::test]
    async fn error_body_has_an_absolute_deadline_despite_progress() {
        let base_url = dribbling_error_server().await;
        let client = XaiClient::new_with_timeouts(&base_url, "key", short_test_timeouts())
            .expect("client")
            .with_retry(RetryConfig {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            });
        let started = std::time::Instant::now();
        let error = client
            .stream(&ResponsesRequest::new("m", vec![]))
            .await
            .expect_err("dribbling error body must hit its total deadline");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "error body deadline was not absolute: {error:?}"
        );
        assert!(
            matches!(&error, XaiError::Server { message, .. } if message.contains("timed out")),
            "unexpected error: {error:?}"
        );
    }
}
