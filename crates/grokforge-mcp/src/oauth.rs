//! OAuth 2.1 authorization for protected Streamable HTTP MCP servers (MCP 2025-11-25).
//!
//! This module deliberately owns no persistence. It returns a binding plus tokens to the host,
//! which stores them in GrokForge's password-encrypted credential file. Discovery and token HTTP
//! clients never follow redirects, never use ambient proxies, and collect only bounded bodies.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use futures::StreamExt as _;
use reqwest::header::{ACCEPT, CONTENT_TYPE, WWW_AUTHENTICATE};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use url::{Host, Url};

const DISCOVERY_MAX_BYTES: usize = 256 * 1024;
const TOKEN_MAX_BYTES: usize = 64 * 1024;
const CHALLENGE_MAX_BYTES: usize = 16 * 1024;
const FIELD_MAX_BYTES: usize = 16 * 1024;
const TOKEN_FIELD_MAX_BYTES: usize = 64 * 1024;
const MAX_AUTHORIZATION_SERVERS: usize = 8;
const MAX_SCOPES: usize = 128;
const CALLBACK_HEAD_MAX_BYTES: usize = 8 * 1024;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const REFRESH_SKEW: Duration = Duration::from_secs(120);

/// Pre-registered OAuth client information from trusted MCP configuration.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthClientConfig {
    pub endpoint: String,
    pub client_id: String,
    pub redirect_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Resolved only from an explicitly named environment variable by the host. It is never
    /// serialized into project configuration or included in diagnostics.
    #[serde(skip, default)]
    pub client_secret: Option<String>,
}

impl std::fmt::Debug for OAuthClientConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthClientConfig")
            .field("endpoint", &"[REDACTED]")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("issuer", &self.issuer.as_ref().map(|_| "[REDACTED]"))
            .field("scopes", &self.scopes)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Stable audience/client binding stored beside the encrypted tokens.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBinding {
    pub endpoint: String,
    pub resource: String,
    pub issuer: String,
    pub client_id: String,
    pub redirect_uri: String,
}

impl std::fmt::Debug for OAuthBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthBinding")
            .field("endpoint", &"[REDACTED]")
            .field("resource", &"[REDACTED]")
            .field("issuer", &"[REDACTED]")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

/// OAuth tokens. Debug output intentionally reports only non-secret metadata.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl std::fmt::Debug for OAuthTokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthTokens")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

impl OAuthTokens {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.expires_at > now_unix().saturating_add(REFRESH_SKEW.as_secs().cast_signed())
    }
}

/// Complete encrypted-storage record for one MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthRecord {
    pub binding: OAuthBinding,
    pub tokens: OAuthTokens,
}

/// Validated discovery output. Endpoint URLs are kept private so callers cannot accidentally
/// persist and later trust them without rediscovery.
#[derive(Clone)]
struct Discovery {
    endpoint: Url,
    resource: Url,
    issuer: Url,
    authorization_endpoint: Url,
    token_endpoint: Url,
    scopes: Vec<String>,
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Discovery")
            .field("endpoint", &"[REDACTED]")
            .field("resource", &"[REDACTED]")
            .field("issuer", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("invalid MCP OAuth configuration: {0}")]
    Configuration(String),
    #[error("MCP OAuth discovery failed: {0}")]
    Discovery(String),
    #[error("MCP OAuth authorization was denied: {0}")]
    Denied(String),
    #[error("MCP OAuth callback timed out")]
    Timeout,
    #[error("MCP OAuth token request failed: {0}")]
    Token(String),
    #[error("MCP OAuth callback I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Dynamic Client Registration is optional in MCP 2025-11-25 and intentionally unsupported in
/// this local-first v1. A pre-registered `client_id` and exact loopback `redirect_uri` are required.
pub const DCR_UNSUPPORTED: &str =
    "dynamic client registration is not supported; configure a pre-registered OAuth client";

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    grant_types_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Challenge {
    resource_metadata: Option<String>,
    scope: Option<String>,
}

struct Pkce {
    verifier: String,
    challenge: String,
}

/// Run discovery, open the authorization page, validate state on the exact loopback callback,
/// exchange the code with PKCE S256 and RFC8707 `resource`, then return encrypted-storage data.
pub async fn login(config: &OAuthClientConfig) -> Result<OAuthRecord, OAuthError> {
    validate_client_config(config)?;
    let discovery = discover(config).await?;
    let redirect = validate_redirect_uri(&config.redirect_uri)?;
    let listener = tokio::net::TcpListener::bind(redirect.0)
        .await
        .map_err(|_| {
            OAuthError::Configuration("pre-registered callback address is unavailable".into())
        })?;
    let pkce = make_pkce()?;
    let state = random_b64(32)?;
    let authorize = authorization_url(config, &discovery, &pkce, &state);
    eprintln!("Opening your browser to authorize MCP access…");
    eprintln!("If it does not open, paste this URL:\n  {authorize}\n");
    open_browser(authorize.as_str());

    let callback = tokio::time::timeout(
        CALLBACK_TIMEOUT,
        accept_callback(listener, redirect.1.as_str(), &state),
    )
    .await
    .map_err(|_| OAuthError::Timeout)??;
    let tokens = exchange_code(config, &discovery, &callback, &pkce.verifier).await;
    let page = if tokens.is_ok() {
        CallbackPage::Success
    } else {
        CallbackPage::Denied
    };
    write_callback_response(callback.socket, page).await;
    Ok(OAuthRecord {
        binding: binding(config, &discovery),
        tokens: tokens?,
    })
}

/// Refresh an expired token after rediscovering and re-verifying the exact endpoint, issuer,
/// resource, client id, and redirect URI binding. Rotated refresh tokens are preserved.
pub async fn refresh(
    config: &OAuthClientConfig,
    record: &OAuthRecord,
) -> Result<OAuthRecord, OAuthError> {
    validate_client_config(config)?;
    let refresh_token =
        record.tokens.refresh_token.as_deref().ok_or_else(|| {
            OAuthError::Token("no refresh token is available; sign in again".into())
        })?;
    let discovery = discover(config).await?;
    let expected = binding(config, &discovery);
    if record.binding != expected {
        return Err(OAuthError::Configuration(
            "stored MCP token binding no longer matches endpoint/issuer/client; sign in again"
                .into(),
        ));
    }
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", config.client_id.clone()),
        ("resource", discovery.resource.as_str().to_string()),
    ];
    if !discovery.scopes.is_empty() {
        form.push(("scope", discovery.scopes.join(" ")));
    }
    let response = token_request(config, &discovery.token_endpoint, &form).await?;
    let mut tokens = tokens_from(response)?;
    if tokens.refresh_token.is_none() {
        tokens
            .refresh_token
            .clone_from(&record.tokens.refresh_token);
    }
    Ok(OAuthRecord {
        binding: expected,
        tokens,
    })
}

/// Check the locally verifiable portion of a stored audience binding before using an unexpired
/// access token. Expired tokens receive the stronger full rediscovery check in [`refresh`].
#[must_use]
pub fn stored_binding_matches(config: &OAuthClientConfig, record: &OAuthRecord) -> bool {
    if validate_client_config(config).is_err() {
        return false;
    }
    let Ok(endpoint) = validate_endpoint(&config.endpoint, "MCP endpoint") else {
        return false;
    };
    let Ok(resource) = validate_endpoint(&record.binding.resource, "stored resource") else {
        return false;
    };
    let Ok(issuer) = validate_endpoint(&record.binding.issuer, "stored issuer") else {
        return false;
    };
    let configured_issuer_matches = config.issuer.as_ref().is_none_or(|configured| {
        validate_endpoint(configured, "configured issuer").is_ok_and(|value| value == issuer)
    });
    record.binding.endpoint == endpoint.as_str()
        && same_origin(&endpoint, &resource)
        && record.binding.client_id == config.client_id
        && record.binding.redirect_uri == config.redirect_uri
        && configured_issuer_matches
        && validate_redirect_uri(&config.redirect_uri).is_ok()
}

fn validate_client_config(config: &OAuthClientConfig) -> Result<(), OAuthError> {
    let _ = validate_endpoint(&config.endpoint, "MCP endpoint")?;
    if config.client_id.is_empty() {
        return Err(OAuthError::Configuration(DCR_UNSUPPORTED.into()));
    }
    validate_field("client_id", &config.client_id)?;
    let _ = validate_redirect_uri(&config.redirect_uri)?;
    if let Some(issuer) = &config.issuer {
        let _ = validate_endpoint(issuer, "configured issuer")?;
    }
    validate_scopes(&config.scopes)?;
    if config
        .client_secret
        .as_ref()
        .is_some_and(|secret| secret.is_empty() || secret.len() > TOKEN_FIELD_MAX_BYTES)
    {
        return Err(OAuthError::Configuration(
            "client secret is empty or exceeds its byte limit".into(),
        ));
    }
    Ok(())
}

async fn discover(config: &OAuthClientConfig) -> Result<Discovery, OAuthError> {
    let endpoint = validate_endpoint(&config.endpoint, "MCP endpoint")?;
    let client = oauth_client()?;
    let challenge = initial_challenge(&client, &endpoint).await?;
    let metadata_urls = protected_metadata_urls(&endpoint, challenge.resource_metadata.as_deref())?;
    let mut protected = None;
    for url in metadata_urls {
        if let Ok(value) =
            fetch_json::<ProtectedResourceMetadata>(&client, &url, DISCOVERY_MAX_BYTES).await
        {
            protected = Some(value);
            break;
        }
    }
    let protected = protected.ok_or_else(|| {
        OAuthError::Discovery("protected-resource metadata was unavailable".into())
    })?;
    if protected.authorization_servers.is_empty()
        || protected.authorization_servers.len() > MAX_AUTHORIZATION_SERVERS
    {
        return Err(OAuthError::Discovery(
            "protected-resource metadata has no bounded authorization server list".into(),
        ));
    }
    let resource = validate_endpoint(&protected.resource, "protected resource")?;
    if !same_origin(&endpoint, &resource) {
        return Err(OAuthError::Discovery(
            "protected resource metadata does not match the MCP endpoint origin".into(),
        ));
    }

    let issuers = protected
        .authorization_servers
        .iter()
        .map(|issuer| validate_endpoint(issuer, "authorization issuer"))
        .collect::<Result<Vec<_>, _>>()?;
    let issuer = select_issuer(config.issuer.as_deref(), &issuers)?;
    let metadata = discover_authorization_server(&client, &issuer).await?;
    let metadata_issuer = validate_endpoint(&metadata.issuer, "metadata issuer")?;
    if metadata_issuer != issuer {
        return Err(OAuthError::Discovery(
            "authorization metadata issuer does not match the discovered issuer".into(),
        ));
    }
    if !metadata
        .code_challenge_methods_supported
        .iter()
        .any(|method| method == "S256")
    {
        return Err(OAuthError::Discovery(
            "authorization server does not advertise PKCE S256".into(),
        ));
    }
    if !metadata.grant_types_supported.is_empty()
        && !metadata
            .grant_types_supported
            .iter()
            .any(|grant| grant == "authorization_code")
    {
        return Err(OAuthError::Discovery(
            "authorization server does not support authorization_code".into(),
        ));
    }
    let authorization_endpoint =
        validate_endpoint(&metadata.authorization_endpoint, "authorization endpoint")?;
    let token_endpoint = validate_endpoint(&metadata.token_endpoint, "token endpoint")?;
    let scopes = if let Some(scope) = challenge.scope {
        parse_scope_string(&scope)?
    } else if !config.scopes.is_empty() {
        config.scopes.clone()
    } else {
        validate_scopes(&protected.scopes_supported)?;
        protected.scopes_supported
    };
    Ok(Discovery {
        endpoint,
        resource,
        issuer,
        authorization_endpoint,
        token_endpoint,
        scopes,
    })
}

fn binding(config: &OAuthClientConfig, discovery: &Discovery) -> OAuthBinding {
    OAuthBinding {
        endpoint: discovery.endpoint.as_str().to_string(),
        resource: discovery.resource.as_str().to_string(),
        issuer: discovery.issuer.as_str().to_string(),
        client_id: config.client_id.clone(),
        redirect_uri: config.redirect_uri.clone(),
    }
}

fn authorization_url(
    config: &OAuthClientConfig,
    discovery: &Discovery,
    pkce: &Pkce,
    state: &str,
) -> Url {
    let mut url = discovery.authorization_endpoint.clone();
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &config.client_id)
            .append_pair("redirect_uri", &config.redirect_uri)
            .append_pair("state", state)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", discovery.resource.as_str());
        if !discovery.scopes.is_empty() {
            query.append_pair("scope", &discovery.scopes.join(" "));
        }
    }
    url
}

async fn exchange_code(
    config: &OAuthClientConfig,
    discovery: &Discovery,
    callback: &AuthorizedCallback,
    verifier: &str,
) -> Result<OAuthTokens, OAuthError> {
    let form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", callback.code.clone()),
        ("redirect_uri", config.redirect_uri.clone()),
        ("client_id", config.client_id.clone()),
        ("code_verifier", verifier.to_string()),
        ("resource", discovery.resource.as_str().to_string()),
    ];
    tokens_from(token_request(config, &discovery.token_endpoint, &form).await?)
}

async fn token_request(
    config: &OAuthClientConfig,
    endpoint: &Url,
    form: &[(&str, String)],
) -> Result<RawTokenResponse, OAuthError> {
    let client = oauth_client()?;
    let mut request = client
        .post(endpoint.clone())
        .header(ACCEPT, "application/json")
        .form(form);
    if let Some(secret) = &config.client_secret {
        request = request.basic_auth(&config.client_id, Some(secret));
    }
    let response = request
        .send()
        .await
        .map_err(|_| OAuthError::Token("token endpoint request failed".into()))?;
    let status = response.status();
    let body = read_bounded(response, TOKEN_MAX_BYTES)
        .await
        .map_err(OAuthError::Token)?;
    if !status.is_success() {
        return Err(OAuthError::Token(format!(
            "token endpoint returned HTTP {}",
            status.as_u16()
        )));
    }
    serde_json::from_slice(&body)
        .map_err(|_| OAuthError::Token("token endpoint returned invalid JSON".into()))
}

fn tokens_from(response: RawTokenResponse) -> Result<OAuthTokens, OAuthError> {
    validate_token_field("access_token", &response.access_token)?;
    if let Some(refresh) = &response.refresh_token {
        validate_token_field("refresh_token", refresh)?;
    }
    if response
        .token_type
        .as_deref()
        .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
    {
        return Err(OAuthError::Token("token_type is not Bearer".into()));
    }
    let expires_in = response.expires_in.unwrap_or(3600);
    if !(1..=31_536_000).contains(&expires_in) {
        return Err(OAuthError::Token(
            "expires_in is outside its safety range".into(),
        ));
    }
    if let Some(scope) = &response.scope {
        let _ = parse_scope_string(scope)?;
    }
    Ok(OAuthTokens {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at: now_unix()
            .checked_add(expires_in)
            .ok_or_else(|| OAuthError::Token("token expiry overflowed".into()))?,
        scope: response.scope,
    })
}

async fn initial_challenge(
    client: &reqwest::Client,
    endpoint: &Url,
) -> Result<Challenge, OAuthError> {
    let body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc":"2.0", "id":1, "method":"initialize", "params": {
            "protocolVersion":"2025-11-25", "capabilities": {},
            "clientInfo":{"name":"grokforge","version":env!("CARGO_PKG_VERSION")}
        }
    }))
    .map_err(|_| OAuthError::Discovery("could not encode discovery request".into()))?;
    let response = client
        .post(endpoint.clone())
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .body(body)
        .send()
        .await
        .map_err(|_| OAuthError::Discovery("MCP authorization challenge request failed".into()))?;
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(Challenge::default());
    }
    parse_www_authenticate(response.headers())
}

fn parse_www_authenticate(headers: &reqwest::header::HeaderMap) -> Result<Challenge, OAuthError> {
    let mut joined = String::new();
    for value in headers.get_all(WWW_AUTHENTICATE) {
        let value = value
            .to_str()
            .map_err(|_| OAuthError::Discovery("WWW-Authenticate is not ASCII".into()))?;
        if !joined.is_empty() {
            joined.push(',');
        }
        joined.push_str(value);
        if joined.len() > CHALLENGE_MAX_BYTES {
            return Err(OAuthError::Discovery(
                "WWW-Authenticate exceeds its byte limit".into(),
            ));
        }
    }
    if joined.is_empty() {
        return Ok(Challenge::default());
    }
    let Some(bearer) = find_bearer_challenge(&joined) else {
        return Ok(Challenge::default());
    };
    Ok(Challenge {
        resource_metadata: challenge_parameter(bearer, "resource_metadata")?,
        scope: challenge_parameter(bearer, "scope")?,
    })
}

fn find_bearer_challenge(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut quoted = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if quoted => {
                index = index.saturating_add(2);
                continue;
            }
            b'"' => quoted = !quoted,
            _ => {}
        }
        let end = index.saturating_add("bearer".len());
        let bearer = !quoted
            && end <= bytes.len()
            && bytes[index..end].eq_ignore_ascii_case(b"bearer")
            && index
                .checked_sub(1)
                .and_then(|previous| bytes.get(previous))
                .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b',')
            && bytes.get(end).is_none_or(u8::is_ascii_whitespace);
        if bearer {
            return Some(value[end..].trim_start());
        }
        index += 1;
    }
    None
}

fn challenge_parameter(value: &str, wanted: &str) -> Result<Option<String>, OAuthError> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b',') {
            index += 1;
        }
        let name_start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'-'))
        {
            index += 1;
        }
        let name = &value[name_start..index];
        while index < bytes.len() && matches!(bytes[index], b' ' | b'\t') {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            // A token not followed by `=` starts the next authentication scheme; parameters after
            // it do not belong to this Bearer challenge.
            return Ok(None);
        }
        index += 1;
        while index < bytes.len() && matches!(bytes[index], b' ' | b'\t') {
            index += 1;
        }
        let parsed = if index < bytes.len() && bytes[index] == b'"' {
            index += 1;
            let mut output = String::new();
            let mut closed = false;
            while index < bytes.len() {
                match bytes[index] {
                    b'"' => {
                        index += 1;
                        closed = true;
                        break;
                    }
                    b'\\' if index + 1 < bytes.len() => {
                        index += 1;
                        output.push(bytes[index] as char);
                        index += 1;
                    }
                    byte if byte.is_ascii_control() => {
                        return Err(OAuthError::Discovery(
                            "invalid WWW-Authenticate quoted value".into(),
                        ));
                    }
                    byte => {
                        output.push(byte as char);
                        index += 1;
                    }
                }
            }
            if !closed {
                return Err(OAuthError::Discovery(
                    "unterminated WWW-Authenticate value".into(),
                ));
            }
            output
        } else {
            let start = index;
            while index < bytes.len() && bytes[index] != b',' && !bytes[index].is_ascii_whitespace()
            {
                index += 1;
            }
            value[start..index].to_string()
        };
        if name.eq_ignore_ascii_case(wanted) {
            if parsed.is_empty() || parsed.len() > FIELD_MAX_BYTES {
                return Err(OAuthError::Discovery(
                    "WWW-Authenticate parameter exceeds its limit".into(),
                ));
            }
            return Ok(Some(parsed));
        }
    }
    Ok(None)
}

fn protected_metadata_urls(
    endpoint: &Url,
    challenge_url: Option<&str>,
) -> Result<Vec<Url>, OAuthError> {
    if let Some(challenge_url) = challenge_url {
        return Ok(vec![validate_endpoint(
            challenge_url,
            "resource metadata URL",
        )?]);
    }
    let mut path_specific = endpoint.clone();
    path_specific.set_query(None);
    path_specific.set_path(&format!(
        "/.well-known/oauth-protected-resource{}",
        endpoint.path().trim_end_matches('/')
    ));
    let mut root = endpoint.clone();
    root.set_query(None);
    root.set_path("/.well-known/oauth-protected-resource");
    if path_specific == root {
        Ok(vec![root])
    } else {
        Ok(vec![path_specific, root])
    }
}

async fn discover_authorization_server(
    client: &reqwest::Client,
    issuer: &Url,
) -> Result<AuthorizationServerMetadata, OAuthError> {
    for url in authorization_metadata_urls(issuer) {
        if let Ok(metadata) = fetch_json(client, &url, DISCOVERY_MAX_BYTES).await {
            return Ok(metadata);
        }
    }
    Err(OAuthError::Discovery(
        "authorization-server metadata was unavailable".into(),
    ))
}

fn authorization_metadata_urls(issuer: &Url) -> Vec<Url> {
    let path = issuer.path().trim_matches('/');
    let mut urls = Vec::new();
    for kind in ["oauth-authorization-server", "openid-configuration"] {
        let mut url = issuer.clone();
        url.set_query(None);
        let suffix = if path.is_empty() {
            String::new()
        } else {
            format!("/{path}")
        };
        url.set_path(&format!("/.well-known/{kind}{suffix}"));
        urls.push(url);
    }
    if !path.is_empty() {
        let mut appended = issuer.clone();
        appended.set_query(None);
        appended.set_path(&format!("/{path}/.well-known/openid-configuration"));
        urls.push(appended);
    }
    urls
}

async fn fetch_json<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &Url,
    limit: usize,
) -> Result<T, OAuthError> {
    let response = client
        .get(url.clone())
        .header(ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| OAuthError::Discovery("metadata request failed".into()))?;
    if !response.status().is_success() {
        return Err(OAuthError::Discovery(format!(
            "metadata endpoint returned HTTP {}",
            response.status().as_u16()
        )));
    }
    let body = read_bounded(response, limit)
        .await
        .map_err(OAuthError::Discovery)?;
    serde_json::from_slice(&body)
        .map_err(|_| OAuthError::Discovery("metadata endpoint returned invalid JSON".into()))
}

async fn read_bounded(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err("response exceeds its byte limit".into());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "response body failed".to_string())?;
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|size| size > limit)
        {
            return Err("response exceeds its byte limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn oauth_client() -> Result<reqwest::Client, OAuthError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .no_proxy()
        .build()
        .map_err(|_| OAuthError::Discovery("could not build secure OAuth client".into()))
}

fn validate_endpoint(value: &str, label: &str) -> Result<Url, OAuthError> {
    if value.is_empty() || value.len() > FIELD_MAX_BYTES {
        return Err(OAuthError::Configuration(format!(
            "{label} exceeds its byte limit"
        )));
    }
    let mut url = Url::parse(value)
        .map_err(|_| OAuthError::Configuration(format!("{label} is not a valid URL")))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(OAuthError::Configuration(format!(
            "{label} must not contain credentials or a fragment"
        )));
    }
    let host = url
        .host()
        .ok_or_else(|| OAuthError::Configuration(format!("{label} has no host")))?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(&host) => {}
        _ => return Err(OAuthError::Configuration(format!("{label} must use HTTPS"))),
    }
    url.set_fragment(None);
    Ok(url)
}

fn is_loopback(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain
            .trim_end_matches('.')
            .eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => IpAddr::V4(*address).is_loopback(),
        Host::Ipv6(address) => IpAddr::V6(*address).is_loopback(),
    }
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str().map(str::to_ascii_lowercase)
            == right.host_str().map(str::to_ascii_lowercase)
        && left.port_or_known_default() == right.port_or_known_default()
}

fn select_issuer(configured: Option<&str>, issuers: &[Url]) -> Result<Url, OAuthError> {
    if let Some(configured) = configured {
        let configured = validate_endpoint(configured, "configured issuer")?;
        return issuers
            .iter()
            .find(|issuer| **issuer == configured)
            .cloned()
            .ok_or_else(|| OAuthError::Discovery("configured issuer was not advertised".into()));
    }
    if issuers.len() != 1 {
        return Err(OAuthError::Configuration(
            "multiple authorization servers were advertised; configure `oauth.issuer`".into(),
        ));
    }
    Ok(issuers[0].clone())
}

fn validate_redirect_uri(value: &str) -> Result<(SocketAddr, String), OAuthError> {
    if value.is_empty() || value.len() > FIELD_MAX_BYTES {
        return Err(OAuthError::Configuration(
            "redirect_uri is empty or exceeds its byte limit".into(),
        ));
    }
    let url = Url::parse(value)
        .map_err(|_| OAuthError::Configuration("redirect_uri is not a valid URL".into()))?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(OAuthError::Configuration(
            "redirect_uri must be a plain HTTP loopback URL without credentials/query/fragment"
                .into(),
        ));
    }
    let ip = match url.host() {
        Some(Host::Ipv4(ip)) if ip.is_loopback() => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) if ip.is_loopback() => IpAddr::V6(ip),
        _ => {
            return Err(OAuthError::Configuration(
                "redirect_uri host must be a literal loopback IP".into(),
            ));
        }
    };
    let port = url.port().filter(|port| *port != 0).ok_or_else(|| {
        OAuthError::Configuration("redirect_uri must include its pre-registered port".into())
    })?;
    let path = url.path().to_string();
    if path.is_empty() || path.len() > 1024 {
        return Err(OAuthError::Configuration(
            "redirect_uri path is invalid".into(),
        ));
    }
    Ok((SocketAddr::new(ip, port), path))
}

fn validate_field(label: &str, value: &str) -> Result<(), OAuthError> {
    if value.is_empty() || value.len() > FIELD_MAX_BYTES || value.chars().any(char::is_control) {
        Err(OAuthError::Configuration(format!(
            "{label} is invalid or exceeds its byte limit"
        )))
    } else {
        Ok(())
    }
}

fn validate_token_field(label: &str, value: &str) -> Result<(), OAuthError> {
    if value.is_empty()
        || value.len() > TOKEN_FIELD_MAX_BYTES
        || value.chars().any(char::is_control)
    {
        Err(OAuthError::Token(format!(
            "{label} is invalid or exceeds its byte limit"
        )))
    } else {
        Ok(())
    }
}

fn validate_scopes(scopes: &[String]) -> Result<(), OAuthError> {
    if scopes.len() > MAX_SCOPES {
        return Err(OAuthError::Configuration(
            "scope list exceeds its limit".into(),
        ));
    }
    for scope in scopes {
        if scope.is_empty()
            || scope.len() > 256
            || scope.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
        {
            return Err(OAuthError::Configuration("OAuth scope is invalid".into()));
        }
    }
    Ok(())
}

fn parse_scope_string(value: &str) -> Result<Vec<String>, OAuthError> {
    let scopes = value
        .split_ascii_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    validate_scopes(&scopes)?;
    Ok(scopes)
}

fn make_pkce() -> Result<Pkce, OAuthError> {
    let verifier = random_b64(32)?;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    Ok(Pkce {
        verifier,
        challenge,
    })
}

fn random_b64(bytes: usize) -> Result<String, OAuthError> {
    let mut random = vec![0_u8; bytes];
    getrandom::getrandom(&mut random)
        .map_err(|_| OAuthError::Configuration("secure randomness is unavailable".into()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration
                .as_secs()
                .min(i64::MAX.cast_unsigned())
                .cast_signed()
        })
}

struct AuthorizedCallback {
    code: String,
    socket: tokio::net::TcpStream,
}

async fn accept_callback(
    listener: tokio::net::TcpListener,
    expected_path: &str,
    expected_state: &str,
) -> Result<AuthorizedCallback, OAuthError> {
    loop {
        let (mut socket, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let Some(request) = read_request_head(&mut socket).await else {
            write_callback_response(socket, CallbackPage::Waiting).await;
            continue;
        };
        let request_line = request.lines().next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        let Ok(target_url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
            write_callback_response(socket, CallbackPage::Waiting).await;
            continue;
        };
        let state = target_url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned());
        if method != "GET"
            || target_url.path() != expected_path
            || state.as_deref() != Some(expected_state)
        {
            write_callback_response(socket, CallbackPage::Waiting).await;
            continue;
        }
        if let Some(error) = target_url
            .query_pairs()
            .find(|(key, _)| key == "error")
            .map(|(_, value)| value.into_owned())
        {
            write_callback_response(socket, CallbackPage::Denied).await;
            return Err(OAuthError::Denied(error.chars().take(256).collect()));
        }
        if let Some(code) = target_url
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
        {
            validate_token_field("authorization code", &code)?;
            return Ok(AuthorizedCallback { code, socket });
        }
        write_callback_response(socket, CallbackPage::Waiting).await;
    }
}

async fn read_request_head(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut body = Vec::new();
    let mut chunk = [0_u8; 1024];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut complete = false;
    while body.len() < CALLBACK_HEAD_MAX_BYTES {
        let remaining = (CALLBACK_HEAD_MAX_BYTES - body.len()).min(chunk.len());
        match tokio::time::timeout_at(deadline, socket.read(&mut chunk[..remaining])).await {
            Ok(Ok(read)) if read > 0 => {
                body.extend_from_slice(&chunk[..read]);
                if body.windows(4).any(|window| window == b"\r\n\r\n") {
                    complete = true;
                    break;
                }
            }
            _ => break,
        }
    }
    complete.then(|| String::from_utf8_lossy(&body).into_owned())
}

#[derive(Clone, Copy)]
enum CallbackPage {
    Success,
    Denied,
    Waiting,
}

async fn write_callback_response(mut socket: tokio::net::TcpStream, page: CallbackPage) {
    let (title, message) = match page {
        CallbackPage::Success => (
            "MCP connected",
            "Authorization succeeded. Return to GrokForge.",
        ),
        CallbackPage::Denied => ("Authorization stopped", "No MCP credentials were saved."),
        CallbackPage::Waiting => (
            "Still waiting",
            "Return to the authorization page and finish signing in.",
        ),
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title><style>body{{background:#08090c;color:#f6f7fb;font:18px system-ui;padding:10vh 12vw}}h1{{color:#ff5a1f}}</style><h1>{title}</h1><p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let command = ("explorer.exe", vec![url]);
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let command = ("xdg-open", vec![url]);
    let _ = std::process::Command::new(command.0)
        .args(command.1)
        .spawn();
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn parses_bearer_resource_metadata_and_authoritative_scope() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            WWW_AUTHENTICATE,
            reqwest::header::HeaderValue::from_static(
                "Bearer realm=\"mcp\", resource_metadata=\"https://mcp.example/.well-known/oauth-protected-resource\", scope=\"files:read files:write\"",
            ),
        );
        let parsed = parse_www_authenticate(&headers).expect("challenge");
        assert_eq!(
            parsed.resource_metadata.as_deref(),
            Some("https://mcp.example/.well-known/oauth-protected-resource")
        );
        assert_eq!(parsed.scope.as_deref(), Some("files:read files:write"));
    }

    #[test]
    fn challenge_parameters_allow_optional_whitespace_around_equals() {
        let value =
            r#"realm = "mcp", resource_metadata = "https://mcp.example/meta", scope = files:read"#;
        assert_eq!(
            challenge_parameter(value, "resource_metadata").expect("metadata"),
            Some("https://mcp.example/meta".into())
        );
        assert_eq!(
            challenge_parameter(value, "scope").expect("scope"),
            Some("files:read".into())
        );
    }

    #[test]
    fn bearer_parser_does_not_take_parameters_from_another_auth_scheme() {
        let tail = find_bearer_challenge(
            r#"Basic realm="not bearer here", Bearer realm="mcp", Basic realm="other", resource_metadata="https://evil.example/meta""#,
        )
        .expect("Bearer challenge");
        assert!(tail.starts_with("realm=\"mcp\""));
        assert_eq!(
            challenge_parameter(tail, "resource_metadata").expect("parse"),
            None
        );
    }

    #[test]
    fn metadata_fallback_and_issuer_path_order_match_the_spec() {
        let endpoint = Url::parse("https://example.com/public/mcp").expect("endpoint");
        let protected = protected_metadata_urls(&endpoint, None).expect("metadata URLs");
        assert_eq!(
            protected[0].path(),
            "/.well-known/oauth-protected-resource/public/mcp"
        );
        assert_eq!(protected[1].path(), "/.well-known/oauth-protected-resource");

        let issuer = Url::parse("https://auth.example.com/tenant1").expect("issuer");
        let auth = authorization_metadata_urls(&issuer);
        assert_eq!(
            auth[0].path(),
            "/.well-known/oauth-authorization-server/tenant1"
        );
        assert_eq!(auth[1].path(), "/.well-known/openid-configuration/tenant1");
        assert_eq!(auth[2].path(), "/tenant1/.well-known/openid-configuration");
    }

    #[test]
    fn pkce_is_s256_and_authorization_includes_resource() {
        let pkce = make_pkce().expect("PKCE");
        assert_eq!(
            pkce.challenge,
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(pkce.verifier.as_bytes()))
        );
        assert_eq!(pkce.verifier.len(), 43);

        let config = OAuthClientConfig {
            endpoint: "https://mcp.example/rpc".into(),
            client_id: "grokforge-client".into(),
            redirect_uri: "http://127.0.0.1:49152/callback".into(),
            issuer: None,
            scopes: vec!["files:read".into()],
            client_secret: None,
        };
        let discovery = Discovery {
            endpoint: Url::parse(&config.endpoint).expect("endpoint"),
            resource: Url::parse("https://mcp.example/").expect("resource"),
            issuer: Url::parse("https://auth.example/").expect("issuer"),
            authorization_endpoint: Url::parse("https://auth.example/authorize?tenant=one")
                .expect("authorize"),
            token_endpoint: Url::parse("https://auth.example/token").expect("token"),
            scopes: config.scopes.clone(),
        };
        let authorize = authorization_url(&config, &discovery, &pkce, "csrf-state");
        let query = authorize.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(query.get("tenant").map(AsRef::as_ref), Some("one"));
        assert_eq!(
            query.get("resource").map(AsRef::as_ref),
            Some("https://mcp.example/")
        );
        assert_eq!(
            query.get("code_challenge_method").map(AsRef::as_ref),
            Some("S256")
        );
        assert_eq!(query.get("state").map(AsRef::as_ref), Some("csrf-state"));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One linear mock records the complete discovery + refresh exchange.
    async fn refresh_discovers_metadata_sends_resource_and_preserves_binding() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let origin = format!("http://{address}");
        let server_origin = origin.clone();
        let server = tokio::spawn(async move {
            for step in 0..4 {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let (method, path, body) = read_mock_request(&mut socket).await;
                match step {
                    0 => {
                        assert_eq!((method.as_str(), path.as_str()), ("POST", "/mcp"));
                        mock_response(
                            &mut socket,
                            "401 Unauthorized",
                            b"",
                            &[&format!(
                                "WWW-Authenticate: Bearer resource_metadata=\"{server_origin}/resource-metadata\", scope=\"files:read\""
                            )],
                        )
                        .await;
                    }
                    1 => {
                        assert_eq!(
                            (method.as_str(), path.as_str()),
                            ("GET", "/resource-metadata")
                        );
                        let payload = serde_json::to_vec(&serde_json::json!({
                            "resource": format!("{server_origin}/mcp"),
                            "authorization_servers": [format!("{server_origin}/issuer")],
                            "scopes_supported": ["ignored:because-challenge-is-authoritative"]
                        }))
                        .expect("resource metadata");
                        mock_response(&mut socket, "200 OK", &payload, &[]).await;
                    }
                    2 => {
                        assert_eq!(
                            (method.as_str(), path.as_str()),
                            ("GET", "/.well-known/oauth-authorization-server/issuer")
                        );
                        let payload = serde_json::to_vec(&serde_json::json!({
                            "issuer": format!("{server_origin}/issuer"),
                            "authorization_endpoint": format!("{server_origin}/authorize"),
                            "token_endpoint": format!("{server_origin}/token"),
                            "code_challenge_methods_supported": ["S256"],
                            "grant_types_supported": ["authorization_code", "refresh_token"]
                        }))
                        .expect("authorization metadata");
                        mock_response(&mut socket, "200 OK", &payload, &[]).await;
                    }
                    3 => {
                        assert_eq!((method.as_str(), path.as_str()), ("POST", "/token"));
                        let form = url::form_urlencoded::parse(&body).collect::<BTreeMap<_, _>>();
                        assert_eq!(
                            form.get("grant_type").map(AsRef::as_ref),
                            Some("refresh_token")
                        );
                        assert_eq!(
                            form.get("refresh_token").map(AsRef::as_ref),
                            Some("old-refresh")
                        );
                        assert_eq!(
                            form.get("resource").map(AsRef::as_ref),
                            Some(format!("{server_origin}/mcp").as_str())
                        );
                        assert_eq!(form.get("scope").map(AsRef::as_ref), Some("files:read"));
                        let payload = br#"{"access_token":"fresh-access","refresh_token":"rotated-refresh","expires_in":3600,"token_type":"Bearer"}"#;
                        mock_response(&mut socket, "200 OK", payload, &[]).await;
                    }
                    _ => unreachable!(),
                }
            }
        });
        let config = OAuthClientConfig {
            endpoint: format!("{origin}/mcp"),
            client_id: "pre-registered-client".into(),
            redirect_uri: "http://127.0.0.1:49152/callback".into(),
            issuer: Some(format!("{origin}/issuer")),
            scopes: Vec::new(),
            client_secret: None,
        };
        let record = OAuthRecord {
            binding: OAuthBinding {
                endpoint: config.endpoint.clone(),
                resource: config.endpoint.clone(),
                issuer: config.issuer.clone().expect("issuer"),
                client_id: config.client_id.clone(),
                redirect_uri: config.redirect_uri.clone(),
            },
            tokens: OAuthTokens {
                access_token: "expired-access".into(),
                refresh_token: Some("old-refresh".into()),
                expires_at: 1,
                scope: Some("files:read".into()),
            },
        };
        let refreshed = refresh(&config, &record).await.expect("refresh");
        assert_eq!(refreshed.binding, record.binding);
        assert_eq!(refreshed.tokens.access_token, "fresh-access");
        assert_eq!(
            refreshed.tokens.refresh_token.as_deref(),
            Some("rotated-refresh")
        );
        server.await.expect("mock server");
    }

    #[tokio::test]
    async fn callback_ignores_wrong_state_then_accepts_the_real_callback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let callback =
            tokio::spawn(async move { accept_callback(listener, "/callback", "correct").await });
        let mut forged = tokio::net::TcpStream::connect(address)
            .await
            .expect("forged");
        forged
            .write_all(b"GET /callback?code=stolen&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write forged");
        let mut response = String::new();
        forged
            .read_to_string(&mut response)
            .await
            .expect("read response");
        assert!(response.contains("Still waiting"));
        assert!(!callback.is_finished());

        let mut real = tokio::net::TcpStream::connect(address).await.expect("real");
        real.write_all(
            b"GET /callback?code=real&state=correct HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .expect("write real");
        let accepted = callback.await.expect("join").expect("accepted");
        assert_eq!(accepted.code, "real");
        write_callback_response(accepted.socket, CallbackPage::Success).await;
    }

    #[tokio::test]
    async fn callback_rejects_an_incomplete_http_request_head() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let reader = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            read_request_head(&mut socket).await
        });
        let mut client = tokio::net::TcpStream::connect(address)
            .await
            .expect("client");
        client
            .write_all(b"GET /callback?code=x&state=y HTTP/1.1\r\nHost: localhost\r\n")
            .await
            .expect("write");
        client.shutdown().await.expect("shutdown");
        assert!(reader.await.expect("reader").is_none());
    }

    async fn read_mock_request(socket: &mut tokio::net::TcpStream) -> (String, String, Vec<u8>) {
        let mut received = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.expect("read request");
            assert!(read > 0, "request closed before headers");
            received.extend_from_slice(&chunk[..read]);
            if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let head = std::str::from_utf8(&received[..header_end]).expect("request headers");
        let request_line = head.lines().next().expect("request line");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().expect("method").to_string();
        let path = parts.next().expect("path").to_string();
        let content_length = head
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
            })
            .unwrap_or(0);
        while received.len().saturating_sub(header_end) < content_length {
            let mut chunk = vec![0_u8; content_length - received.len().saturating_sub(header_end)];
            socket.read_exact(&mut chunk).await.expect("request body");
            received.extend_from_slice(&chunk);
        }
        (
            method,
            path,
            received[header_end..header_end + content_length].to_vec(),
        )
    }

    async fn mock_response(
        socket: &mut tokio::net::TcpStream,
        status: &str,
        body: &[u8],
        headers: &[&str],
    ) {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for header in headers {
            response.push_str(header);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        socket
            .write_all(response.as_bytes())
            .await
            .expect("headers");
        socket.write_all(body).await.expect("body");
    }

    #[test]
    fn token_debug_never_contains_secrets() {
        let tokens = OAuthTokens {
            access_token: "secret-access".into(),
            refresh_token: Some("secret-refresh".into()),
            expires_at: 42,
            scope: None,
        };
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("secret-access"));
        assert!(!debug.contains("secret-refresh"));
    }
}
