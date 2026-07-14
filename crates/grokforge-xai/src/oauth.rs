//! xAI OAuth for SuperGrok / X Premium+ subscribers — the loopback authorization-code + PKCE
//! flow. Signing in this way bills inference against the user's **subscription** instead of a
//! pay-per-token API key. The resulting bearer token is used against the same `api.x.ai/v1`
//! endpoint the API-key path uses.
//!
//! The client id below is xAI's **public** desktop OAuth client (shared by the Grok CLI and
//! other tools — it is metadata, not a secret). Endpoints match the OIDC discovery document at
//! `https://auth.x.ai/.well-known/openid-configuration`.
//!
//! Note: as of this writing xAI's backend restricts OAuth inference to the SuperGrok **Heavy**
//! tier; standard SuperGrok / X Premium+ subscribers may receive a 403 until xAI lifts that.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const PREFERRED_PORT: u16 = 56121;
/// Refresh a little before the token actually expires.
const REFRESH_SKEW: Duration = Duration::from_secs(120);
/// OAuth token responses are tiny JSON documents. Bound the collected body so a broken or
/// compromised token endpoint cannot consume unbounded memory before parsing fails.
const TOKEN_RESPONSE_MAX_BYTES: usize = 64 * 1024;

/// Errors from the OAuth flow.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("could not bind a local callback port")]
    Bind,
    #[error("the browser sign-in was not completed in time")]
    Timeout,
    #[error("sign-in was cancelled or denied: {0}")]
    Denied(String),
    #[error("token request failed: {0}")]
    Token(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

/// Stored subscription tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds at which the access token expires.
    pub expires_at: i64,
}

impl OAuthTokens {
    /// Whether the access token is still usable (with a refresh-early skew).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let now = now_unix() + i64::try_from(REFRESH_SKEW.as_secs()).unwrap_or(0);
        self.expires_at > now
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 32 bytes of randomness from two v4 UUIDs (no extra RNG dependency).
fn random_32() -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    out[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    out
}

struct Pkce {
    verifier: String,
    challenge: String,
}

fn make_pkce() -> Pkce {
    let verifier = b64url(&random_32());
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

fn tokens_from(resp: TokenResponse) -> Result<OAuthTokens, OAuthError> {
    if resp.access_token.trim().is_empty() {
        return Err(OAuthError::Token(
            "token response contained an empty access_token".to_string(),
        ));
    }
    let expires_in = resp.expires_in.unwrap_or(3600);
    if expires_in <= 0 {
        return Err(OAuthError::Token(
            "token response contained a non-positive expires_in".to_string(),
        ));
    }
    let expires_at = now_unix().checked_add(expires_in).ok_or_else(|| {
        OAuthError::Token("token expiry is outside the supported range".to_string())
    })?;
    Ok(OAuthTokens {
        access_token: resp.access_token,
        refresh_token: resp.refresh_token,
        expires_at,
    })
}

/// Run the interactive browser sign-in. Prints (and tries to open) the authorization URL,
/// waits for the loopback callback, and exchanges the code for subscription tokens.
pub async fn login() -> Result<OAuthTokens, OAuthError> {
    // Bind the loopback callback listener (preferred port, else an ephemeral one).
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", PREFERRED_PORT)).await {
        Ok(l) => l,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| OAuthError::Bind)?,
    };
    let port = listener.local_addr().map_err(|_| OAuthError::Bind)?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let pkce = make_pkce();
    let state = b64url(&random_32());

    let authorize = url::Url::parse_with_params(
        AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", &redirect_uri),
            ("scope", SCOPE),
            ("state", &state),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("referrer", "hermes-agent"),
        ],
    )
    .map_err(|e| OAuthError::Token(e.to_string()))?;

    eprintln!("Opening your browser to sign in to xAI (SuperGrok / X Premium+)…");
    eprintln!("If it doesn't open, paste this URL:\n  {authorize}\n");
    open_browser(authorize.as_str());

    // Wait for the redirect (with a generous timeout).
    let mut callback =
        tokio::time::timeout(Duration::from_secs(300), accept_callback(listener, &state))
            .await
            .map_err(|_| OAuthError::Timeout)??;

    // Only claim that GrokForge is signed in after the authorization code has actually become a
    // usable token. Keep the browser connection open for this short exchange so a rejected code
    // cannot produce a false-success page.
    let tokens = exchange_code(&callback.code, &redirect_uri, &pkce.verifier).await;
    let page = if tokens.is_ok() {
        CallbackPage::Success
    } else {
        CallbackPage::Denied
    };
    write_callback_response(&mut callback.socket, page).await;
    tokens
}

/// Build the HTTP client used for the OAuth token endpoints. It applies a connect timeout and an
/// overall request timeout so a stuck or malicious `auth.x.ai` cannot hang credential refresh (and
/// thus the whole agent loop) indefinitely. Mirrors the timeout strategy of the main API client.
fn token_client() -> Result<reqwest::Client, OAuthError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        // Authorization codes, PKCE verifiers, and refresh tokens live in the form body. A 307 or
        // 308 redirect can replay that body, so never follow token-endpoint redirects.
        .redirect(reqwest::redirect::Policy::none())
        // Proxy environment variables are not an explicit GrokForge credential-egress boundary.
        .no_proxy()
        .build()
        .map_err(OAuthError::Http)
}

/// Exchange a refresh token for a fresh access token.
pub async fn refresh(refresh_token: &str) -> Result<OAuthTokens, OAuthError> {
    let client = token_client()?;
    let resp = client
        .post(TOKEN_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await?;
    parse_token_response(resp).await
}

async fn exchange_code(
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<OAuthTokens, OAuthError> {
    let client = token_client()?;
    let resp = client
        .post(TOKEN_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await?;
    parse_token_response(resp).await
}

async fn parse_token_response(resp: reqwest::Response) -> Result<OAuthTokens, OAuthError> {
    let status = resp.status();
    if resp
        .content_length()
        .is_some_and(|length| length > TOKEN_RESPONSE_MAX_BYTES as u64)
    {
        return Err(OAuthError::Token(format!(
            "token response exceeds the {TOKEN_RESPONSE_MAX_BYTES}-byte safety limit"
        )));
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > TOKEN_RESPONSE_MAX_BYTES {
            return Err(OAuthError::Token(format!(
                "token response exceeds the {TOKEN_RESPONSE_MAX_BYTES}-byte safety limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return Err(OAuthError::Token(format!(
            "{status}: {}",
            String::from_utf8_lossy(&body)
                .chars()
                .take(300)
                .collect::<String>()
        )));
    }
    let parsed: TokenResponse =
        serde_json::from_slice(&body).map_err(|e| OAuthError::Token(e.to_string()))?;
    tokens_from(parsed)
}

/// Wait for the loopback redirect carrying `code` + `state`. The success page is only shown when
/// both values are present and the state matches. Browsers (Safari especially) open speculative
/// or favicon connections that arrive *before* the real redirect and carry no `code`; we must
/// answer and ignore those, and keep accepting until the real one arrives — otherwise the
/// listener closes and the redirect hits a dead port.
async fn accept_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<AuthorizedCallback, OAuthError> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let request = read_request_head(&mut sock).await;
        let target = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        let (code, state, error) =
            parse_callback_query(target.split_once('?').map_or("", |(_, q)| q));

        let page = callback_status_page(
            code.as_deref(),
            state.as_deref(),
            error.as_deref(),
            expected_state,
        );

        // A browser or local process can connect to this loopback port. Only a callback carrying
        // our unguessable OAuth state is terminal; unrelated errors/codes are answered but ignored
        // so they cannot race and cancel the real sign-in flow.
        if state.as_deref() != Some(expected_state) {
            write_callback_response(&mut sock, page).await;
            continue;
        }
        if let Some(err) = error {
            write_callback_response(&mut sock, CallbackPage::Denied).await;
            return Err(OAuthError::Denied(err));
        }
        if let Some(c) = code {
            return Ok(AuthorizedCallback {
                code: c,
                socket: sock,
            });
        }
        write_callback_response(&mut sock, page).await;
        // Non-callback connection (preconnect / favicon / no code) — keep waiting.
    }
}

struct AuthorizedCallback {
    code: String,
    socket: tokio::net::TcpStream,
}

async fn write_callback_response(socket: &mut tokio::net::TcpStream, page: CallbackPage) {
    let response = callback_http_response(page);
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallbackPage {
    Success,
    Denied,
    Waiting,
}

fn callback_status_page(
    code: Option<&str>,
    state: Option<&str>,
    error: Option<&str>,
    expected_state: &str,
) -> CallbackPage {
    if state != Some(expected_state) {
        CallbackPage::Waiting
    } else if error.is_some() {
        CallbackPage::Denied
    } else if code.is_some() {
        CallbackPage::Success
    } else {
        CallbackPage::Waiting
    }
}

impl CallbackPage {
    fn copy(
        self,
    ) -> (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ) {
        match self {
            Self::Success => (
                "success",
                "Authorized",
                "xAI authorization complete",
                "Back to the forge.",
                "xAI approved this session. Return to your terminal to finish secure local setup and confirm whether your credentials were saved.",
                "Close this tab, then follow the final status in your terminal.",
            ),
            Self::Denied => (
                "denied",
                "Sign-in stopped",
                "Authentication stopped",
                "Forge paused.",
                "xAI cancelled or could not complete authorization. Your terminal has the details, and no new credentials were saved.",
                "Close this tab and return to your terminal to try again.",
            ),
            Self::Waiting => (
                "waiting",
                "Waiting for sign-in",
                "Authorization pending",
                "Finish with xAI.",
                "This is the local callback, not the xAI sign-in window. Complete authorization with xAI; GrokForge is still waiting in your terminal.",
                "This page will not update. Return to xAI to finish signing in, or check your terminal for details.",
            ),
        }
    }

    fn terminal_line(self) -> &'static str {
        match self {
            Self::Success => "xAI authorization complete",
            Self::Denied => "authorization not completed",
            Self::Waiting => "awaiting authorization",
        }
    }

    fn terminal_icon(self) -> &'static str {
        match self {
            Self::Success => "✓",
            Self::Denied => "×",
            Self::Waiting => "·",
        }
    }

    fn terminal_state(self) -> &'static str {
        match self {
            Self::Success => "finish in terminal",
            Self::Denied => "return to terminal",
            Self::Waiting => "terminal still waiting",
        }
    }
}

fn callback_http_response(page: CallbackPage) -> String {
    let body = callback_page(page);
    format!(
        concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "Content-Length: {}\r\n",
            "Cache-Control: no-store, max-age=0\r\n",
            "Pragma: no-cache\r\n",
            "Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; ",
            "img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n",
            "Permissions-Policy: camera=(), microphone=(), geolocation=()\r\n",
            "Referrer-Policy: no-referrer\r\n",
            "X-Content-Type-Options: nosniff\r\n",
            "X-Frame-Options: DENY\r\n",
            "Connection: close\r\n\r\n",
            "{}"
        ),
        body.len(),
        body
    )
}

fn callback_page(page: CallbackPage) -> String {
    let (state, title, eyebrow, heading, message, handoff) = page.copy();
    CALLBACK_PAGE_TEMPLATE
        .replace("__STATE__", state)
        .replace("__TITLE__", title)
        .replace("__EYEBROW__", eyebrow)
        .replace("__HEADING__", heading)
        .replace("__MESSAGE__", message)
        .replace("__HANDOFF__", handoff)
        .replace("__TERMINAL_ICON__", page.terminal_icon())
        .replace("__TERMINAL_LINE__", page.terminal_line())
        .replace("__TERMINAL_STATE__", page.terminal_state())
}

const CALLBACK_PAGE_TEMPLATE: &str = include_str!("oauth_callback.html");

/// Read an HTTP request head (up to the blank line), tolerating idle/speculative connections
/// via a short per-connection read timeout so a silent preconnect can't stall the flow.
async fn read_request_head(sock: &mut tokio::net::TcpStream) -> String {
    const MAX_HEAD_BYTES: usize = 8192;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    // A single absolute deadline for the whole head, so a slow-drip client cannot reset a
    // per-read timeout indefinitely and pin the (local-only) loopback accept loop.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        // Enforce the size cap before reading so the buffer never overshoots by a full chunk.
        if buf.len() >= MAX_HEAD_BYTES {
            break;
        }
        let remaining = MAX_HEAD_BYTES - buf.len();
        let read_len = remaining.min(tmp.len());
        match tokio::time::timeout_at(deadline, sock.read(&mut tmp[..read_len])).await {
            Ok(Ok(n)) if n > 0 => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            // EOF, read error, or the absolute deadline elapsed — stop reading this connection.
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Extract `(code, state, error)` from a callback query string.
fn parse_callback_query(query: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (mut code, mut state, mut error) = (None, None, None);
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v);
        match k {
            "code" => code = Some(v),
            "state" => state = Some(v),
            "error" => error = Some(v),
            _ => {}
        }
    }
    (code, state, error)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.replace('+', " ");
    let mut out = Vec::new();
    let mut it = bytes.bytes();
    while let Some(b) = it.next() {
        if b == b'%'
            && let Some(a) = it.next()
            && let Some(c) = it.next()
            && let Ok(byte) = u8::from_str_radix(&format!("{}{}", a as char, c as char), 16)
        {
            out.push(byte);
        } else {
            out.push(b);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let cmd = ("cmd", vec!["/C", "start", url]);
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let cmd = ("xdg-open", vec![url]);
    let _ = std::process::Command::new(cmd.0).args(cmd.1).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let p = make_pkce();
        let expected = b64url(&Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expected);
        // base64url no-pad 32-byte verifier -> 43 chars.
        assert_eq!(p.verifier.len(), 43);
    }

    #[test]
    fn token_validity_uses_expiry() {
        let valid = OAuthTokens {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: now_unix() + 3600,
        };
        assert!(valid.is_valid());
        let expired = OAuthTokens {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: now_unix() - 10,
        };
        assert!(!expired.is_valid());
    }

    #[test]
    fn token_response_requires_a_usable_token_and_bounded_expiry() {
        assert!(
            tokens_from(TokenResponse {
                access_token: "   ".to_string(),
                refresh_token: None,
                expires_in: Some(3600),
            })
            .is_err()
        );
        assert!(
            tokens_from(TokenResponse {
                access_token: "access".to_string(),
                refresh_token: None,
                expires_in: Some(0),
            })
            .is_err()
        );
        assert!(
            tokens_from(TokenResponse {
                access_token: "access".to_string(),
                refresh_token: None,
                expires_in: Some(i64::MAX),
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn token_client_does_not_follow_redirects_with_secret_form_bodies() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind redirect server");
        let address = listener.local_addr().expect("redirect server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("initial token request");
            let mut request = vec![0u8; 4096];
            let read = socket.read(&mut request).await.expect("read token request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("refresh_token=secret-refresh"));
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{address}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write redirect");
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_ok()
        });

        let response = token_client()
            .expect("token client")
            .post(format!("http://{address}/token"))
            .form(&[("refresh_token", "secret-refresh")])
            .send()
            .await
            .expect("redirect response");
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert!(!server.await.expect("redirect server task"));
    }

    #[tokio::test]
    async fn token_response_body_is_bounded() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind token server");
        let address = listener.local_addr().expect("token server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("token request");
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await.expect("read token request");
            let body = vec![b'x'; TOKEN_RESPONSE_MAX_BYTES + 1];
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write token headers");
            // The client may reject from Content-Length before consuming the body and close early.
            let _ = socket.write_all(&body).await;
        });

        let response = token_client()
            .expect("token client")
            .get(format!("http://{address}/token"))
            .send()
            .await
            .expect("token response");
        let error = parse_token_response(response)
            .await
            .expect_err("oversized token body must fail");
        assert!(error.to_string().contains("safety limit"));
        server.await.expect("token server task");
    }

    #[tokio::test]
    async fn callback_request_head_stops_at_the_exact_byte_cap() {
        const EXPECTED_CAP: usize = 8192;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind callback reader");
        let address = listener.local_addr().expect("callback reader address");
        let reader = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("callback connection");
            read_request_head(&mut socket).await
        });

        let mut client = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect callback reader");
        let oversized = vec![b'a'; EXPECTED_CAP + 4096];
        let _ = client.write_all(&oversized).await;
        let _ = client.shutdown().await;

        let request = reader.await.expect("callback reader task");
        assert_eq!(request.len(), EXPECTED_CAP);
    }

    #[test]
    fn percent_decode_handles_encodings() {
        assert_eq!(percent_decode("a%2Fb%20c"), "a/b c");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn parses_callback_and_ignores_non_callbacks() {
        let (code, state, error) = parse_callback_query("state=abc&code=xyz");
        assert_eq!(code.as_deref(), Some("xyz"));
        assert_eq!(state.as_deref(), Some("abc"));
        assert!(error.is_none());

        // A preconnect / favicon request has no code -> caller keeps waiting.
        let (code, _, _) = parse_callback_query("");
        assert!(code.is_none());

        let (_, _, error) = parse_callback_query("error=access_denied");
        assert_eq!(error.as_deref(), Some("access_denied"));
    }

    #[test]
    fn callback_pages_are_branded_and_state_specific() {
        let success = callback_page(CallbackPage::Success);
        assert!(success.starts_with("<!doctype html>"));
        assert!(success.contains("data-state=\"success\""));
        assert!(success.contains("Back to the forge"));
        assert!(success.contains("confirm whether your credentials were saved"));
        assert!(success.contains("xAI authorization complete"));
        assert!(!success.contains("You're all set"));
        assert!(!success.contains("ready to build"));
        assert!(success.contains("fill=\"#ff5a1f\""));
        assert!(success.contains("prefers-reduced-motion"));
        assert!(success.contains("<kbd class=\"key\">⌘W</kbd>"));
        assert!(success.contains("<kbd class=\"key\">Ctrl+W</kbd>"));
        assert!(success.contains("follow the final status in your terminal"));

        let denied = callback_page(CallbackPage::Denied);
        assert!(denied.contains("data-state=\"denied\""));
        assert!(denied.contains("no new credentials were saved"));
        assert!(denied.contains("authorization not completed"));
        assert!(denied.contains("return to terminal"));
        assert!(denied.contains("return to your terminal to try again"));

        let waiting = callback_page(CallbackPage::Waiting);
        assert!(waiting.contains("data-state=\"waiting\""));
        assert!(waiting.contains("not the xAI sign-in window"));
        assert!(waiting.contains("terminal still waiting"));
        assert!(waiting.contains("This page will not update"));

        for page in [&success, &denied, &waiting] {
            assert!(page.contains("Served locally by GrokForge at 127.0.0.1"));
            assert!(page.contains("This page loads no remote assets"));
        }

        for page in [success, denied, waiting] {
            assert!(
                !page.contains("__"),
                "callback page contains an unreplaced template sentinel"
            );
        }
    }

    #[test]
    fn callback_only_claims_success_for_matching_state() {
        assert_eq!(
            callback_status_page(Some("code"), Some("expected"), None, "expected"),
            CallbackPage::Success
        );
        assert_eq!(
            callback_status_page(Some("code"), Some("attacker"), None, "expected"),
            CallbackPage::Waiting
        );
        assert_eq!(
            callback_status_page(None, None, None, "expected"),
            CallbackPage::Waiting
        );
        assert_eq!(
            callback_status_page(None, None, Some("access_denied"), "expected"),
            CallbackPage::Waiting
        );
        assert_eq!(
            callback_status_page(None, Some("expected"), Some("access_denied"), "expected"),
            CallbackPage::Denied
        );
    }

    #[tokio::test]
    async fn callback_ignores_forged_state_and_defers_success_until_token_exchange() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind callback listener");
        let address = listener.local_addr().expect("callback address");
        let callback = tokio::spawn(async move { accept_callback(listener, "expected").await });

        let mut forged = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect forged callback");
        forged
            .write_all(
                b"GET /callback?error=access_denied&state=attacker HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .await
            .expect("write forged callback");
        let mut forged_response = String::new();
        forged
            .read_to_string(&mut forged_response)
            .await
            .expect("read forged response");
        assert!(forged_response.contains("data-state=\"waiting\""));
        assert!(!callback.is_finished());

        let mut browser = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect real callback");
        browser
            .write_all(
                b"GET /callback?code=real-code&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .await
            .expect("write real callback");
        let mut authorized = callback
            .await
            .expect("callback task")
            .expect("authorized callback");
        assert_eq!(authorized.code, "real-code");

        let mut first_byte = [0u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), browser.read(&mut first_byte))
                .await
                .is_err(),
            "success must wait until the caller completes token exchange"
        );

        write_callback_response(&mut authorized.socket, CallbackPage::Success).await;
        let mut response = String::new();
        browser
            .read_to_string(&mut response)
            .await
            .expect("read success response");
        assert!(response.contains("data-state=\"success\""));
    }

    #[test]
    fn callback_page_is_self_contained_and_hardened() {
        let response = callback_http_response(CallbackPage::Success);
        let (headers, body) = response
            .split_once("\r\n\r\n")
            .expect("response has an HTTP header terminator");

        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(headers.contains("Content-Type: text/html; charset=utf-8"));
        assert!(headers.contains("Cache-Control: no-store"));
        assert!(headers.contains("Content-Security-Policy: default-src 'none'"));
        assert!(headers.contains("frame-ancestors 'none'"));
        assert!(headers.contains("Referrer-Policy: no-referrer"));
        assert!(headers.contains("X-Content-Type-Options: nosniff"));

        let declared_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .expect("response declares a content length")
            .parse::<usize>()
            .expect("content length is numeric");
        assert_eq!(declared_length, body.len());

        assert!(!body.contains("<script"));
        assert!(!body.contains("src=\"http"));
        assert!(!body.contains("href=\"http"));
        assert!(body.contains("href=\"data:image/svg+xml"));
        assert!(body.contains(".visual { min-height: 18.5rem;"));
        assert!(body.contains(".visual { min-height: 18rem;"));
    }
}
