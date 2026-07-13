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

fn tokens_from(resp: TokenResponse) -> OAuthTokens {
    OAuthTokens {
        access_token: resp.access_token,
        refresh_token: resp.refresh_token,
        expires_at: now_unix() + resp.expires_in.unwrap_or(3600),
    }
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
    let (code, got_state) =
        tokio::time::timeout(Duration::from_secs(300), accept_callback(listener))
            .await
            .map_err(|_| OAuthError::Timeout)??;
    if got_state != state {
        return Err(OAuthError::Denied(
            "state mismatch (possible CSRF)".to_string(),
        ));
    }

    exchange_code(&code, &redirect_uri, &pkce.verifier).await
}

/// Exchange a refresh token for a fresh access token.
pub async fn refresh(refresh_token: &str) -> Result<OAuthTokens, OAuthError> {
    let client = reqwest::Client::new();
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
    let client = reqwest::Client::new();
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
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(OAuthError::Token(format!(
            "{status}: {}",
            body.chars().take(300).collect::<String>()
        )));
    }
    let parsed: TokenResponse =
        serde_json::from_str(&body).map_err(|e| OAuthError::Token(e.to_string()))?;
    Ok(tokens_from(parsed))
}

/// Wait for the loopback redirect carrying `code` + `state`. Browsers (Safari especially) open
/// speculative/preconnect and favicon connections that arrive *before* the real redirect and
/// carry no `code`; we must answer and ignore those, and keep accepting until the real one
/// arrives — otherwise the listener closes and the redirect hits a dead port.
async fn accept_callback(
    listener: tokio::net::TcpListener,
) -> Result<(String, String), OAuthError> {
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

        let done = code.is_some() || error.is_some();
        let heading = if error.is_some() {
            "Sign-in was cancelled. You can close this tab."
        } else if done {
            "GrokForge is signed in. You can close this tab and return to the terminal."
        } else {
            "Waiting for GrokForge sign-in…"
        };
        let body = format!(
            "<html><body style=\"font-family:sans-serif\"><h3>{heading}</h3></body></html>"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.shutdown().await;

        if let Some(err) = error {
            return Err(OAuthError::Denied(err));
        }
        if let (Some(c), Some(s)) = (code, state) {
            return Ok((c, s));
        }
        // Non-callback connection (preconnect / favicon / no code) — keep waiting.
    }
}

/// Read an HTTP request head (up to the blank line), tolerating idle/speculative connections
/// via a short per-connection read timeout so a silent preconnect can't stall the flow.
async fn read_request_head(sock: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut tmp)).await {
            Ok(Ok(n)) if n > 0 => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 8192 {
                    break;
                }
            }
            // EOF, read error, or idle-connection timeout — stop reading this connection.
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
}
