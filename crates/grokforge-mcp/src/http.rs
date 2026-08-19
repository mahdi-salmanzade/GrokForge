//! MCP Streamable HTTP transport. Redirects are disabled, response bodies are streamed through a
//! hard byte cap, and custom headers are treated as secrets and never included in diagnostics.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use url::{Host, Url};

use super::{
    DEFAULT_REQUEST_TIMEOUT, JsonRpcTransport, LATEST_PROTOCOL_VERSION, McpConnection, McpError,
    McpTool, SUPPORTED_PROTOCOL_VERSIONS, call_tool_via, list_tools_via,
};

const MAX_ENDPOINT_BYTES: usize = 8 * 1024;
const MAX_HEADERS: usize = 32;
const MAX_HEADER_NAME_BYTES: usize = 128;
const MAX_HEADER_VALUE_BYTES: usize = 16 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_SESSION_ID_BYTES: usize = 1024;
const MAX_HTTP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MCP_SESSION_ID: HeaderName = HeaderName::from_static("mcp-session-id");
const MCP_PROTOCOL_VERSION: HeaderName = HeaderName::from_static("mcp-protocol-version");

/// Non-secret accounting metadata emitted immediately before an MCP HTTP body is handed to the
/// HTTP client. `body_bytes` is the exact serialized JSON request-body length; the body and custom
/// headers are deliberately not exposed to the observer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEgress {
    pub method: String,
    pub body_bytes: usize,
}

/// Hook used by the host to feed MCP HTTP egress accounting into its context ledger/event stream.
pub type RemoteEgressObserver = Arc<dyn Fn(RemoteEgress) + Send + Sync>;

/// Validate a Streamable HTTP endpoint without making a network request.
///
/// Remote endpoints must use HTTPS. Plain HTTP is accepted only for a literal loopback address or
/// `localhost`, which keeps local development convenient without sending MCP data or credentials
/// over a clear-text network connection.
pub fn validate_remote_url(endpoint: &str) -> Result<Url, McpError> {
    if endpoint.is_empty() || endpoint.len() > MAX_ENDPOINT_BYTES {
        return Err(McpError::Protocol(
            "MCP HTTP endpoint is empty or exceeds its byte limit".to_string(),
        ));
    }
    let url = Url::parse(endpoint)
        .map_err(|_| McpError::Protocol("MCP HTTP endpoint is not a valid URL".to_string()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(McpError::Protocol(
            "MCP HTTP endpoint must not contain user credentials".to_string(),
        ));
    }
    if url.fragment().is_some() {
        return Err(McpError::Protocol(
            "MCP HTTP endpoint must not contain a fragment".to_string(),
        ));
    }
    let host = url
        .host()
        .ok_or_else(|| McpError::Protocol("MCP HTTP endpoint must include a host".to_string()))?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(&host) => {}
        "http" => {
            return Err(McpError::Protocol(
                "plain HTTP MCP endpoints are allowed only on loopback".to_string(),
            ));
        }
        _ => {
            return Err(McpError::Protocol(
                "MCP HTTP endpoint must use https (or http on loopback)".to_string(),
            ));
        }
    }
    Ok(url)
}

fn is_loopback_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain
            .trim_end_matches('.')
            .eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => IpAddr::V4(*address).is_loopback(),
        Host::Ipv6(address) => IpAddr::V6(*address).is_loopback(),
    }
}

#[derive(Default)]
struct HttpState {
    next_id: u64,
    session_id: Option<HeaderValue>,
    protocol_version: Option<HeaderValue>,
}

/// An MCP server reached using the Streamable HTTP transport.
pub struct StreamableHttpClient {
    name: String,
    endpoint: Url,
    client: reqwest::Client,
    state: Mutex<HttpState>,
    request_timeout: Duration,
    supports_tools: bool,
    egress_observer: Option<RemoteEgressObserver>,
}

impl std::fmt::Debug for StreamableHttpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The endpoint may contain a secret query parameter and custom headers routinely contain
        // bearer tokens, so neither belongs in Debug output.
        formatter
            .debug_struct("StreamableHttpClient")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl StreamableHttpClient {
    /// Connect and complete the MCP initialize handshake.
    pub async fn connect(
        name: &str,
        endpoint: &str,
        headers: &[(String, String)],
    ) -> Result<Self, McpError> {
        Self::connect_with_options(name, endpoint, headers, DEFAULT_REQUEST_TIMEOUT, None).await
    }

    /// Connect with a non-secret egress accounting hook. The hook receives only the JSON-RPC
    /// method and exact serialized body length, never request content or headers.
    pub async fn connect_with_egress_observer(
        name: &str,
        endpoint: &str,
        headers: &[(String, String)],
        observer: RemoteEgressObserver,
    ) -> Result<Self, McpError> {
        Self::connect_with_options(
            name,
            endpoint,
            headers,
            DEFAULT_REQUEST_TIMEOUT,
            Some(observer),
        )
        .await
    }

    /// Connect with an OAuth Bearer token supplied by the trusted host. This separate entry point
    /// prevents project header configuration from silently replacing the encrypted OAuth token.
    pub async fn connect_with_oauth_token(
        name: &str,
        endpoint: &str,
        headers: &[(String, String)],
        access_token: &str,
    ) -> Result<Self, McpError> {
        Self::connect_with_auth_options(
            name,
            endpoint,
            headers,
            Some(access_token),
            DEFAULT_REQUEST_TIMEOUT,
            None,
        )
        .await
    }

    /// OAuth variant with non-secret remote egress accounting.
    pub async fn connect_with_oauth_token_and_egress_observer(
        name: &str,
        endpoint: &str,
        headers: &[(String, String)],
        access_token: &str,
        observer: RemoteEgressObserver,
    ) -> Result<Self, McpError> {
        Self::connect_with_auth_options(
            name,
            endpoint,
            headers,
            Some(access_token),
            DEFAULT_REQUEST_TIMEOUT,
            Some(observer),
        )
        .await
    }

    async fn connect_with_options(
        name: &str,
        endpoint: &str,
        headers: &[(String, String)],
        request_timeout: Duration,
        egress_observer: Option<RemoteEgressObserver>,
    ) -> Result<Self, McpError> {
        Self::connect_with_auth_options(
            name,
            endpoint,
            headers,
            None,
            request_timeout,
            egress_observer,
        )
        .await
    }

    async fn connect_with_auth_options(
        name: &str,
        endpoint: &str,
        headers: &[(String, String)],
        oauth_token: Option<&str>,
        request_timeout: Duration,
        egress_observer: Option<RemoteEgressObserver>,
    ) -> Result<Self, McpError> {
        if request_timeout.is_zero() || request_timeout > MAX_HTTP_TIMEOUT {
            return Err(McpError::Protocol(
                "MCP HTTP timeout is outside the supported range".to_string(),
            ));
        }
        let endpoint = validate_remote_url(endpoint)?;
        let mut default_headers = validated_headers(headers)?;
        if let Some(token) = oauth_token {
            if default_headers.contains_key(AUTHORIZATION) {
                return Err(McpError::Protocol(
                    "OAuth MCP configuration must not also set an Authorization header".into(),
                ));
            }
            let bearer = bearer_header(token)?;
            default_headers.insert(AUTHORIZATION, bearer);
        }
        let mut builder = reqwest::Client::builder()
            .default_headers(default_headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(request_timeout.min(Duration::from_secs(10)))
            .timeout(request_timeout);
        // A loopback request must not be routed through an ambient HTTP proxy.
        if endpoint.scheme() == "http" {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|_| McpError::Http("could not build the HTTP client"))?;
        let mut connection = Self {
            name: name.to_string(),
            endpoint,
            client,
            state: Mutex::new(HttpState {
                next_id: 1,
                ..HttpState::default()
            }),
            request_timeout,
            supports_tools: false,
            egress_observer,
        };
        connection.supports_tools = connection.initialize().await?;
        Ok(connection)
    }

    async fn initialize(&self) -> Result<bool, McpError> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "grokforge", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        let protocol = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::Protocol("initialize omitted protocolVersion".to_string()))?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&protocol) {
            return Err(McpError::Protocol(format!(
                "server negotiated unsupported protocol version `{protocol}`"
            )));
        }
        if !result.get("capabilities").is_some_and(Value::is_object)
            || !result.get("serverInfo").is_some_and(Value::is_object)
        {
            return Err(McpError::Protocol(
                "initialize omitted required capabilities or serverInfo".to_string(),
            ));
        }
        let mut protocol_header = HeaderValue::from_str(protocol)
            .map_err(|_| McpError::Protocol("invalid negotiated protocol version".to_string()))?;
        protocol_header.set_sensitive(true);
        self.state.lock().await.protocol_version = Some(protocol_header);
        let supports_tools = result
            .pointer("/capabilities/tools")
            .is_some_and(Value::is_object);
        self.notify("notifications/initialized", json!({})).await?;
        Ok(supports_tools)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let body = serialize_http_json(&message)?;
        let operation = async {
            let state = self.state.lock().await;
            let response = self.post(method, &body, &state).await?;
            if !response.status().is_success() {
                return Err(McpError::HttpStatus(response.status().as_u16()));
            }
            // Notifications normally receive 202 with no body. Drain any non-empty success body
            // through the same cap so a broken server cannot force an unbounded allocation.
            let _ = read_response_body(response).await?;
            Ok(())
        };
        tokio::time::timeout(self.request_timeout, operation)
            .await
            .map_err(|_| McpError::Timeout {
                method: method.to_string(),
                timeout: self.request_timeout,
            })?
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        tokio::time::timeout(self.request_timeout, self.request_inner(method, params))
            .await
            .map_err(|_| McpError::Timeout {
                method: method.to_string(),
                timeout: self.request_timeout,
            })?
    }

    async fn request_inner(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let mut state = self.state.lock().await;
        let id = state.next_id;
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| McpError::Protocol("MCP request id space exhausted".to_string()))?;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let body = serialize_http_json(&message)?;
        let response = self.post(method, &body, &state).await?;
        if !response.status().is_success() {
            return Err(McpError::HttpStatus(response.status().as_u16()));
        }
        if response.status() == reqwest::StatusCode::ACCEPTED {
            return Err(McpError::Protocol(
                "MCP request received notification-only HTTP 202 response".to_string(),
            ));
        }
        if method == "initialize" {
            state.session_id = session_id(response.headers())?;
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| McpError::Protocol("MCP HTTP response omitted Content-Type".into()))?;
        let bytes = read_response_body(response).await?;
        let messages = match content_type.as_str() {
            "application/json" => vec![serde_json::from_slice(&bytes)?],
            "text/event-stream" => parse_sse(&bytes)?,
            _ => {
                return Err(McpError::Protocol(
                    "MCP HTTP response used an unsupported Content-Type".to_string(),
                ));
            }
        };
        matching_result(messages, id)
    }

    async fn post(
        &self,
        method: &str,
        body: &[u8],
        state: &HttpState,
    ) -> Result<reqwest::Response, McpError> {
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header(USER_AGENT, concat!("grokforge/", env!("CARGO_PKG_VERSION")))
            .body(body.to_vec());
        if let Some(session_id) = &state.session_id {
            request = request.header(MCP_SESSION_ID, session_id.clone());
        }
        if let Some(protocol_version) = &state.protocol_version {
            request = request.header(MCP_PROTOCOL_VERSION, protocol_version.clone());
        }
        if let Some(observer) = &self.egress_observer {
            observer(RemoteEgress {
                method: method.to_string(),
                body_bytes: body.len(),
            });
        }
        request.send().await.map_err(|error| {
            if error.is_timeout() {
                McpError::Timeout {
                    method: method.to_string(),
                    timeout: self.request_timeout,
                }
            } else {
                McpError::Http("request failed")
            }
        })
    }
}

fn bearer_header(token: &str) -> Result<HeaderValue, McpError> {
    if token.is_empty()
        || token.len() > MAX_HEADER_VALUE_BYTES.saturating_sub("Bearer ".len())
        || token
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(McpError::Protocol(
            "MCP OAuth access token is invalid or exceeds its byte limit".into(),
        ));
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| McpError::Protocol("invalid MCP OAuth access token".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

#[async_trait]
impl JsonRpcTransport for StreamableHttpClient {
    async fn rpc_request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        self.request(method, params).await
    }

    fn supports_tools(&self) -> bool {
        self.supports_tools
    }

    fn request_timeout(&self) -> Duration {
        self.request_timeout
    }
}

#[async_trait]
impl McpConnection for StreamableHttpClient {
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        list_tools_via(self).await
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<String, McpError> {
        call_tool_via(self, name, args).await
    }
}

fn validated_headers(headers: &[(String, String)]) -> Result<HeaderMap, McpError> {
    if headers.len() > MAX_HEADERS {
        return Err(McpError::Protocol(format!(
            "MCP HTTP headers exceed the {MAX_HEADERS}-header limit"
        )));
    }
    let mut total = 0_usize;
    let mut names = BTreeSet::new();
    let mut result = HeaderMap::new();
    for (name, value) in headers {
        if name.is_empty()
            || name.len() > MAX_HEADER_NAME_BYTES
            || value.len() > MAX_HEADER_VALUE_BYTES
        {
            return Err(McpError::Protocol(
                "MCP HTTP header exceeds its byte limit".to_string(),
            ));
        }
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| McpError::Protocol("invalid MCP HTTP header name".to_string()))?;
        if reserved_header(&header_name) {
            return Err(McpError::Protocol(
                "MCP HTTP configuration attempted to override a transport header".to_string(),
            ));
        }
        if !names.insert(header_name.as_str().to_string()) {
            return Err(McpError::Protocol(
                "MCP HTTP header names must be unique (case-insensitively)".to_string(),
            ));
        }
        total = total
            .checked_add(name.len())
            .and_then(|size| size.checked_add(value.len()))
            .ok_or_else(|| McpError::Protocol("MCP HTTP headers are too large".to_string()))?;
        if total > MAX_HEADER_BYTES {
            return Err(McpError::Protocol(format!(
                "MCP HTTP headers exceed the {MAX_HEADER_BYTES}-byte limit"
            )));
        }
        let mut header_value = HeaderValue::from_str(value)
            .map_err(|_| McpError::Protocol("invalid MCP HTTP header value".to_string()))?;
        header_value.set_sensitive(true);
        result.insert(header_name, header_value);
    }
    Ok(result)
}

fn reserved_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "accept"
            | "connection"
            | "content-length"
            | "content-type"
            | "host"
            | "mcp-protocol-version"
            | "mcp-session-id"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "user-agent"
    )
}

fn session_id(headers: &HeaderMap) -> Result<Option<HeaderValue>, McpError> {
    let Some(value) = headers.get(&MCP_SESSION_ID) else {
        return Ok(None);
    };
    if value.is_empty() || value.as_bytes().len() > MAX_SESSION_ID_BYTES {
        return Err(McpError::Protocol(
            "MCP session id is empty or exceeds its byte limit".to_string(),
        ));
    }
    let mut value = value.clone();
    value.set_sensitive(true);
    Ok(Some(value))
}

async fn read_response_body(response: reqwest::Response) -> Result<Vec<u8>, McpError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_BODY_BYTES as u64)
    {
        return Err(McpError::Protocol(format!(
            "MCP HTTP response exceeded {MAX_HTTP_BODY_BYTES} bytes"
        )));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| McpError::Http("response body failed"))?;
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_HTTP_BODY_BYTES)
        {
            return Err(McpError::Protocol(format!(
                "MCP HTTP response exceeded {MAX_HTTP_BODY_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn serialize_http_json(message: &Value) -> Result<Vec<u8>, McpError> {
    struct CappedBody {
        bytes: Vec<u8>,
        exceeded: bool,
    }

    impl std::io::Write for CappedBody {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self
                .bytes
                .len()
                .checked_add(bytes.len())
                .is_none_or(|length| length > MAX_HTTP_BODY_BYTES)
            {
                self.exceeded = true;
                return Err(std::io::Error::other("MCP HTTP request exceeds byte cap"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut body = CappedBody {
        bytes: Vec::new(),
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut body, message) {
        if body.exceeded {
            return Err(McpError::Protocol(format!(
                "MCP HTTP request exceeded {MAX_HTTP_BODY_BYTES} bytes"
            )));
        }
        return Err(McpError::Decode(error));
    }
    Ok(body.bytes)
}

fn parse_sse(bytes: &[u8]) -> Result<Vec<Value>, McpError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| McpError::Protocol("MCP event stream was not valid UTF-8".to_string()))?;
    let mut messages = Vec::new();
    let mut data = Vec::new();
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !data.is_empty() {
                let payload = data.join("\n");
                messages.push(serde_json::from_str(&payload)?);
                data.clear();
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if messages.is_empty() {
        return Err(McpError::Protocol(
            "MCP event stream contained no JSON-RPC message".to_string(),
        ));
    }
    Ok(messages)
}

fn matching_result(messages: Vec<Value>, id: u64) -> Result<Value, McpError> {
    for message in messages {
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(McpError::Protocol(
                "MCP HTTP response is not JSON-RPC 2.0".to_string(),
            ));
        }
        if message.get("method").is_some() && message.get("id").is_some() {
            return Err(McpError::Protocol(
                "server-initiated requests are not supported by the bounded HTTP transport"
                    .to_string(),
            ));
        }
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = message.get("error") {
            let code = error.get("code").and_then(Value::as_i64);
            return Err(McpError::Rpc(code.map_or_else(
                || "remote JSON-RPC error".to_string(),
                |code| format!("remote JSON-RPC error code {code}"),
            )));
        }
        return message.get("result").cloned().ok_or_else(|| {
            McpError::Protocol(
                "matching JSON-RPC response omitted both result and error".to_string(),
            )
        });
    }
    Err(McpError::Protocol(
        "MCP HTTP response omitted the matching JSON-RPC response".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};

    #[test]
    fn endpoint_policy_requires_https_or_loopback() {
        assert!(validate_remote_url("https://mcp.example.test/rpc").is_ok());
        assert!(validate_remote_url("http://127.0.0.1:9000/rpc").is_ok());
        assert!(validate_remote_url("http://[::1]:9000/rpc").is_ok());
        assert!(validate_remote_url("http://localhost:9000/rpc").is_ok());
        assert!(validate_remote_url("http://example.test/rpc").is_err());
        assert!(validate_remote_url("https://token@example.test/rpc").is_err());
        assert!(validate_remote_url("file:///tmp/mcp.sock").is_err());
    }

    #[test]
    fn secret_headers_are_sensitive_and_transport_headers_are_reserved() {
        let headers = validated_headers(&[(
            "Authorization".to_string(),
            "Bearer very-secret-value".to_string(),
        )])
        .unwrap();
        assert!(headers.get("authorization").unwrap().is_sensitive());
        assert!(format!("{headers:?}").contains("Sensitive"));
        assert!(
            validated_headers(&[("Content-Type".to_string(), "text/plain".to_string())]).is_err()
        );
        assert!(
            validated_headers(&[
                ("X-Key".to_string(), "one".to_string()),
                ("x-key".to_string(), "two".to_string())
            ])
            .is_err()
        );
    }

    #[test]
    fn oauth_bearer_headers_are_sensitive_and_strict() {
        let bearer = bearer_header("oauth-secret").unwrap();
        assert!(bearer.is_sensitive());
        assert_eq!(bearer.to_str().unwrap(), "Bearer oauth-secret");
        assert!(bearer_header("").is_err());
        assert!(bearer_header("contains space").is_err());
        assert!(bearer_header("contains\nnewline").is_err());
    }

    #[tokio::test]
    async fn oauth_rejects_a_project_authorization_header_before_network() {
        let error = StreamableHttpClient::connect_with_auth_options(
            "collision",
            "http://127.0.0.1:9/mcp",
            &[("Authorization".into(), "Bearer project-secret".into())],
            Some("encrypted-oauth-secret"),
            Duration::from_secs(1),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            McpError::Protocol(message) if message.contains("must not also set")
        ));
    }

    #[test]
    fn parses_sse_and_matches_json_rpc_response() {
        let messages = parse_sse(
            b": keepalive\n\ndata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":7,\"result\":{\"ok\":true}}\n\n",
        )
        .unwrap();
        let result = matching_result(messages, 7).unwrap();
        assert_eq!(result, json!({ "ok": true }));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn initializes_lists_and_calls_over_streamable_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for step in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, message) = read_request(&mut stream).await;
                assert_eq!(
                    headers.get("authorization").map(String::as_str),
                    Some("Bearer test-secret")
                );
                if step == 0 {
                    assert!(!headers.contains_key("mcp-session-id"));
                    assert!(!headers.contains_key("mcp-protocol-version"));
                } else {
                    assert_eq!(
                        headers.get("mcp-session-id").map(String::as_str),
                        Some("session-123")
                    );
                    assert_eq!(
                        headers.get("mcp-protocol-version").map(String::as_str),
                        Some("2025-11-25")
                    );
                }
                let method = message.get("method").and_then(Value::as_str).unwrap();
                match method {
                    "initialize" => {
                        let id = message["id"].as_u64().unwrap();
                        respond_json(
                            &mut stream,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "protocolVersion": "2025-11-25",
                                    "capabilities": { "tools": {} },
                                    "serverInfo": { "name": "test", "version": "1" }
                                }
                            }),
                            &["Mcp-Session-Id: session-123"],
                        )
                        .await;
                    }
                    "notifications/initialized" => respond_accepted(&mut stream).await,
                    "tools/list" => {
                        let id = message["id"].as_u64().unwrap();
                        let event = format!(
                            "data: {}\n\n",
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "tools": [{
                                    "name": "echo",
                                    "description": "echo text",
                                    "inputSchema": { "type": "object" }
                                }] }
                            })
                        );
                        respond(
                            &mut stream,
                            "200 OK",
                            "text/event-stream",
                            event.as_bytes(),
                            &[],
                        )
                        .await;
                    }
                    "tools/call" => {
                        let id = message["id"].as_u64().unwrap();
                        respond_json(
                            &mut stream,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "content": [{ "type": "text", "text": "remote ok" }] }
                            }),
                            &[],
                        )
                        .await;
                    }
                    other => panic!("unexpected method: {other}"),
                }
            }
        });

        let egress_events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_for_hook = Arc::clone(&egress_events);
        let accounting_hook: RemoteEgressObserver = Arc::new(move |event| {
            events_for_hook.lock().unwrap().push(event);
        });
        let client = StreamableHttpClient::connect_with_options(
            "remote",
            &format!("http://{address}/mcp"),
            &[(
                "Authorization".to_string(),
                "Bearer test-secret".to_string(),
            )],
            Duration::from_secs(2),
            Some(accounting_hook),
        )
        .await
        .unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(
            client
                .call_tool("echo", json!({ "text": "hello" }))
                .await
                .unwrap(),
            "remote ok"
        );
        server.await.unwrap();

        let events = egress_events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.method.as_str())
                .collect::<Vec<_>>(),
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
        let expected_initialize = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": LATEST_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "grokforge", "version": env!("CARGO_PKG_VERSION") }
            }
        }))
        .unwrap()
        .len();
        assert_eq!(events[0].body_bytes, expected_initialize);
        assert!(events.iter().all(|event| event.body_bytes > 0));
    }

    #[tokio::test]
    async fn http_request_and_response_sizes_are_bounded() {
        let oversized = json!({ "value": "x".repeat(MAX_HTTP_BODY_BYTES) });
        assert!(matches!(
            serialize_http_json(&oversized),
            Err(McpError::Protocol(message)) if message.contains("request exceeded")
        ));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_HTTP_BODY_BYTES + 1
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let error = StreamableHttpClient::connect_with_options(
            "oversized",
            &format!("http://{address}/mcp"),
            &[],
            Duration::from_secs(2),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            McpError::Protocol(message) if message.contains("response exceeded")
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_initialization_timeout_is_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let error = StreamableHttpClient::connect_with_options(
            "hung",
            &format!("http://{address}/mcp"),
            &[],
            Duration::from_millis(30),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, McpError::Timeout { method, .. } if method == "initialize"));
        server.await.unwrap();
    }

    async fn read_request(
        stream: &mut TcpStream,
    ) -> (std::collections::BTreeMap<String, String>, Value) {
        let mut received = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0, "connection closed before HTTP headers");
            received.extend_from_slice(&chunk[..count]);
            assert!(
                received.len() <= 64 * 1024,
                "test request headers too large"
            );
            if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let header_text = std::str::from_utf8(&received[..header_end]).unwrap();
        let mut headers = std::collections::BTreeMap::new();
        for line in header_text
            .split("\r\n")
            .skip(1)
            .filter(|line| !line.is_empty())
        {
            let (name, value) = line.split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
        }
        let length = headers["content-length"].parse::<usize>().unwrap();
        while received.len() - header_end < length {
            let mut chunk = vec![0_u8; length - (received.len() - header_end)];
            stream.read_exact(&mut chunk).await.unwrap();
            received.extend_from_slice(&chunk);
        }
        let message = serde_json::from_slice(&received[header_end..header_end + length]).unwrap();
        (headers, message)
    }

    async fn respond_json(stream: &mut TcpStream, value: &Value, headers: &[&str]) {
        let body = serde_json::to_vec(value).unwrap();
        respond(stream, "200 OK", "application/json", &body, headers).await;
    }

    async fn respond_accepted(stream: &mut TcpStream) {
        stream
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    }

    async fn respond(
        stream: &mut TcpStream,
        status: &str,
        content_type: &str,
        body: &[u8],
        headers: &[&str],
    ) {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for header in headers {
            response.push_str(header);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
    }
}
