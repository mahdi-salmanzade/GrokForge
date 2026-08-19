//! Authenticated, bounded local HTTP API for GrokForge.
//!
//! The server is deliberately small: a public health check and OpenAPI document, authenticated
//! session metadata reads, and an authenticated prompt endpoint that streams the same typed
//! protocol events as the terminal frontends. It adds no CORS policy, public tunnel, telemetry,
//! or remote control plane.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use futures::Stream;
use grokforge_core::{
    Agent, AllowRule, AutoApprover, BoundedEventQueueStatus, Session, SessionConfig, SessionMeta,
    ToolRegistry, TurnCancellation,
};
use grokforge_protocol::{ApprovalPolicy, EventMsg, NetworkMode, SandboxMode, SessionId};
use grokforge_sandbox::default_runner;
use grokforge_xai::{Effort, XaiClient};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, Semaphore, mpsc, oneshot};
use zeroize::Zeroize as _;

/// The checked-in API contract returned by `/openapi.json`.
pub const OPENAPI_JSON: &str = include_str!("../openapi.json");

const MIN_TOKEN_BYTES: usize = 32;
const MAX_TOKEN_BYTES: usize = 1_024;
const MAX_PROMPT_BYTES: usize = 32 * 1_024;
const MAX_SESSION_RESULTS: usize = 100;
const MAX_SESSION_ID_BYTES: usize = 64;

/// Resource limits applied at the HTTP boundary.
#[derive(Debug, Clone)]
pub struct ApiLimits {
    pub max_request_body_bytes: usize,
    pub max_concurrent_prompts: usize,
    pub max_stream_events: usize,
    pub max_stream_bytes: usize,
    pub max_event_bytes: usize,
    pub max_request_duration: Duration,
}

impl Default for ApiLimits {
    fn default() -> Self {
        Self {
            max_request_body_bytes: 64 * 1_024,
            max_concurrent_prompts: 1,
            max_stream_events: 32_768,
            max_stream_bytes: 16 * 1_024 * 1_024,
            max_event_bytes: 1_024 * 1_024,
            max_request_duration: Duration::from_secs(30 * 60),
        }
    }
}

impl ApiLimits {
    fn validate(&self) -> Result<(), ServerError> {
        if !(1..=1024 * 1024).contains(&self.max_request_body_bytes) {
            return Err(ServerError::InvalidConfiguration(
                "request body cap must be between 1 byte and 1 MiB".to_string(),
            ));
        }
        if !(1..=64).contains(&self.max_concurrent_prompts) {
            return Err(ServerError::InvalidConfiguration(
                "prompt concurrency must be between 1 and 64".to_string(),
            ));
        }
        if self.max_stream_events == 0
            || self.max_stream_bytes == 0
            || self.max_event_bytes == 0
            || self.max_event_bytes > self.max_stream_bytes
            || self.max_request_duration.is_zero()
        {
            return Err(ServerError::InvalidConfiguration(
                "stream and duration limits must be non-zero and internally consistent".to_string(),
            ));
        }
        Ok(())
    }
}

/// Authentication state stores only a digest, never the reusable bearer token.
#[derive(Clone)]
pub struct ServerAuth {
    token_digest: [u8; 32],
}

impl std::fmt::Debug for ServerAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerAuth")
            .field("token_digest", &"[REDACTED]")
            .finish()
    }
}

impl ServerAuth {
    /// Validate and hash a configured bearer token.
    pub fn new(token: &str) -> Result<Self, ServerError> {
        validate_token(token)?;
        Ok(Self {
            token_digest: Sha256::digest(token.as_bytes()).into(),
        })
    }

    fn permits(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers.get(AUTHORIZATION) else {
            return false;
        };
        let Ok(value) = value.to_str() else {
            return false;
        };
        let Some(token) = value.strip_prefix("Bearer ") else {
            return false;
        };
        let presented: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        constant_time_equal(&self.token_digest, &presented)
    }
}

/// A resolved server token. `token` is returned so the CLI can show an ephemeral loopback token
/// once; application state receives only `auth` and does not retain the plaintext.
pub struct ResolvedAuth {
    pub auth: ServerAuth,
    pub token: String,
    pub generated: bool,
}

impl std::fmt::Debug for ResolvedAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedAuth")
            .field("auth", &self.auth)
            .field("token", &"[REDACTED]")
            .field("generated", &self.generated)
            .finish()
    }
}

impl Drop for ResolvedAuth {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

/// Resolve authentication for a bind address. The v1 transport is plain HTTP, so even a strong
/// bearer token would be observable on a non-loopback network. Remote access belongs behind a TLS
/// reverse proxy connected to this loopback-only service.
pub fn resolve_auth(
    bind_ip: IpAddr,
    configured_token: Option<String>,
) -> Result<ResolvedAuth, ServerError> {
    if !bind_ip.is_loopback() {
        return Err(ServerError::InvalidConfiguration(
            "the local API is loopback-only; use a TLS reverse proxy for remote access".to_string(),
        ));
    }
    let (mut token, generated) = match configured_token {
        Some(token) => (token, false),
        None => (generate_token()?, true),
    };
    let auth = match ServerAuth::new(&token) {
        Ok(auth) => auth,
        Err(error) => {
            token.zeroize();
            return Err(error);
        }
    };
    Ok(ResolvedAuth {
        auth,
        token,
        generated,
    })
}

fn validate_token(token: &str) -> Result<(), ServerError> {
    if !(MIN_TOKEN_BYTES..=MAX_TOKEN_BYTES).contains(&token.len()) {
        return Err(ServerError::InvalidConfiguration(format!(
            "server token must contain {MIN_TOKEN_BYTES}-{MAX_TOKEN_BYTES} bytes"
        )));
    }
    if token
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(ServerError::InvalidConfiguration(
            "server token must not contain whitespace or control characters".to_string(),
        ));
    }
    // Length alone would accept obvious placeholders such as 32 repeated characters. Random
    // base64/hex secrets comfortably exceed this diversity floor, while low-entropy examples do
    // not. This is a guardrail rather than an entropy estimator.
    let mut seen = [false; 256];
    for byte in token.bytes() {
        seen[usize::from(byte)] = true;
    }
    if seen.into_iter().filter(|present| *present).count() < 12 {
        return Err(ServerError::InvalidConfiguration(
            "server token appears low-entropy; use a cryptographically random value".to_string(),
        ));
    }
    Ok(())
}

fn generate_token() -> Result<String, ServerError> {
    let mut random = [0_u8; 32];
    getrandom::getrandom(&mut random).map_err(|error| ServerError::Entropy(error.to_string()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random))
}

fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

/// JSON body accepted by the programmatic prompt endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptRequest {
    pub prompt: String,
    #[serde(default)]
    pub plan: bool,
}

impl PromptRequest {
    fn validate(&self) -> Result<(), RunError> {
        if self.prompt.trim().is_empty() {
            return Err(RunError::InvalidPrompt(
                "prompt must not be empty".to_string(),
            ));
        }
        if self.prompt.len() > MAX_PROMPT_BYTES {
            return Err(RunError::InvalidPrompt(format!(
                "prompt exceeds the {MAX_PROMPT_BYTES}-byte limit"
            )));
        }
        if self
            .prompt
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            return Err(RunError::InvalidPrompt(
                "prompt contains unsupported control characters".to_string(),
            ));
        }
        Ok(())
    }
}

/// A started prompt and the bounded frontend side of its event stream.
pub struct StartedPrompt {
    pub session_id: SessionId,
    pub events: mpsc::Receiver<EventMsg>,
    pub cancellation: TurnCancellation,
    /// Present for bounded production queues so the HTTP stream can report an overflow even when
    /// the event that detected it could not itself be enqueued.
    pub event_queue_status: Option<BoundedEventQueueStatus>,
    /// Resolves only after the runner has finished cooperative cancellation/timeout cleanup.
    /// The HTTP admission permit is retained until this closes, even if the client disconnects.
    pub completion: oneshot::Receiver<()>,
}

impl std::fmt::Debug for StartedPrompt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StartedPrompt")
            .field("session_id", &self.session_id)
            .field("cancellation", &self.cancellation)
            .field("event_queue_status", &self.event_queue_status)
            .finish_non_exhaustive()
    }
}

/// Backend used by the HTTP boundary. The production implementation below runs the real agent;
/// the seam also makes authentication and transport integration tests deterministic.
#[async_trait]
pub trait PromptRunner: Send + Sync {
    async fn start(&self, request: PromptRequest) -> Result<StartedPrompt, RunError>;
}

/// Runtime values shared by all prompts served by one process.
#[derive(Clone)]
pub struct ProductionConfig {
    pub workspace: PathBuf,
    pub sessions_dir: PathBuf,
    pub client: XaiClient,
    pub model: String,
    pub plan_model: String,
    pub effort: Option<Effort>,
    pub context_window_tokens: Option<u64>,
    pub plan_context_window_tokens: Option<u64>,
    pub approval_policy: ApprovalPolicy,
    pub sandbox_mode: SandboxMode,
    pub allow: Vec<AllowRule>,
    pub max_iterations: u32,
    pub auto_compact: bool,
    pub compaction_trigger_bytes: usize,
    pub compaction_keep_tail: usize,
    pub trust_project_mcp: bool,
    /// Decrypted only by the host binary and kept out of project configuration and Debug output.
    pub mcp_oauth_tokens: BTreeMap<String, String>,
    pub trust_project_tools: bool,
    pub event_queue_capacity: usize,
    pub turn_timeout: Duration,
}

impl std::fmt::Debug for ProductionConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProductionConfig")
            .field("workspace", &self.workspace)
            .field("sessions_dir", &self.sessions_dir)
            .field("client", &self.client)
            .field("model", &self.model)
            .field("plan_model", &self.plan_model)
            .field("effort", &self.effort)
            .field("approval_policy", &self.approval_policy)
            .field("sandbox_mode", &self.sandbox_mode)
            .field("max_iterations", &self.max_iterations)
            .field("trust_project_mcp", &self.trust_project_mcp)
            .field("trust_project_tools", &self.trust_project_tools)
            .finish_non_exhaustive()
    }
}

impl ProductionConfig {
    #[must_use]
    pub fn new(
        workspace: PathBuf,
        sessions_dir: PathBuf,
        client: XaiClient,
        model: String,
    ) -> Self {
        Self {
            workspace,
            sessions_dir,
            client,
            plan_model: model.clone(),
            model,
            effort: None,
            context_window_tokens: None,
            plan_context_window_tokens: None,
            approval_policy: ApprovalPolicy::OnRequest,
            sandbox_mode: SandboxMode::WorkspaceWrite,
            allow: Vec::new(),
            max_iterations: 32,
            auto_compact: true,
            compaction_trigger_bytes: 400_000,
            compaction_keep_tail: 8,
            trust_project_mcp: false,
            mcp_oauth_tokens: BTreeMap::new(),
            trust_project_tools: false,
            event_queue_capacity: 128,
            turn_timeout: Duration::from_secs(30 * 60),
        }
    }

    fn validate(&self) -> Result<(), RunError> {
        if !self.workspace.is_absolute() || !self.workspace.is_dir() {
            return Err(RunError::Configuration(
                "workspace must be an existing absolute directory".to_string(),
            ));
        }
        let valid_model = |model: &str| {
            !model.is_empty()
                && model.len() <= 160
                && model.trim() == model
                && !model.chars().any(char::is_whitespace)
                && !model.chars().any(char::is_control)
        };
        if !valid_model(&self.model) || !valid_model(&self.plan_model) {
            return Err(RunError::Configuration(
                "execute or plan model slug is empty or exceeds its safety limit".to_string(),
            ));
        }
        if !(1..=256).contains(&self.max_iterations) {
            return Err(RunError::Configuration(
                "max iterations must be between 1 and 256".to_string(),
            ));
        }
        if !(1..=1024).contains(&self.event_queue_capacity) || self.turn_timeout.is_zero() {
            return Err(RunError::Configuration(
                "event queue and turn timeout must be bounded and non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

/// Real GrokForge agent runner used by `grokforge serve`.
#[derive(Debug, Clone)]
pub struct ProductionRunner {
    config: Arc<ProductionConfig>,
    /// All requests target one workspace. Execute turns take the write side while plan turns take
    /// the read side, so configured HTTP concurrency can never create overlapping mutations or
    /// let a plan observe a half-applied execute turn.
    workspace_gate: Arc<RwLock<()>>,
}

struct WorkspacePermit {
    _read: Option<OwnedRwLockReadGuard<()>>,
    _write: Option<OwnedRwLockWriteGuard<()>>,
}

impl ProductionRunner {
    pub fn new(config: ProductionConfig) -> Result<Self, RunError> {
        config.validate()?;
        Ok(Self {
            config: Arc::new(config),
            workspace_gate: Arc::new(RwLock::new(())),
        })
    }

    fn model_settings(&self, plan: bool) -> (&str, Option<Effort>, Option<u64>) {
        if plan {
            (
                &self.config.plan_model,
                Some(Effort::High),
                self.config.plan_context_window_tokens,
            )
        } else {
            (
                &self.config.model,
                self.config.effort,
                self.config.context_window_tokens,
            )
        }
    }

    /// Add project-provided executable capabilities only to execute requests. Starting a trusted
    /// stdio MCP server can itself mutate the host during initialization, and a project custom
    /// tool marked non-mutating is still an external program. A plan request must not even load
    /// those executable surfaces if it is to preserve its process-level read-only boundary.
    async fn register_project_capabilities(
        &self,
        plan: bool,
        registry: &mut ToolRegistry,
        events: &mpsc::UnboundedSender<EventMsg>,
    ) {
        if plan {
            return;
        }

        let custom_tools = grokforge_core::tools::custom::register_custom_tools(
            &self.config.workspace,
            self.config.trust_project_tools,
            registry,
        );
        for warning in custom_tools.warnings {
            let _ = events.send(EventMsg::Error {
                message: format!("custom tools: {warning}"),
                recoverable: true,
            });
        }
        if self.config.trust_project_mcp {
            let _ = grokforge_core::mcp_config::connect_and_register_trusted_with_events_and_oauth(
                &self.config.workspace,
                registry,
                Some(events.clone()),
                &self.config.mcp_oauth_tokens,
            )
            .await;
        } else {
            let _ =
                grokforge_core::mcp_config::connect_and_register(&self.config.workspace, registry)
                    .await;
        }
    }
}

#[async_trait]
impl PromptRunner for ProductionRunner {
    #[allow(clippy::too_many_lines)] // Linear setup makes persistence, registry trust, cancellation, and spawn ordering auditable.
    async fn start(&self, request: PromptRequest) -> Result<StartedPrompt, RunError> {
        request.validate()?;

        // Acquire before loading executable project configuration or starting MCP processes, and
        // move the owned guard into the turn task. `max_concurrent_prompts` therefore increases
        // concurrent read-only planning throughput without weakening single-writer ownership.
        let workspace_permit = if request.plan {
            WorkspacePermit {
                _read: Some(Arc::clone(&self.workspace_gate).read_owned().await),
                _write: None,
            }
        } else {
            WorkspacePermit {
                _read: None,
                _write: Some(Arc::clone(&self.workspace_gate).write_owned().await),
            }
        };

        let (model, effort, context_window_tokens) = self.model_settings(request.plan);
        let mut session_config =
            SessionConfig::new(self.config.workspace.clone(), model.to_string())
                .with_policy(self.config.approval_policy, self.config.sandbox_mode);
        if self
            .config
            .allow
            .iter()
            .any(|rule| matches!(rule, AllowRule::Network | AllowRule::All))
        {
            session_config.network = NetworkMode::Full;
        }
        session_config.effort = effort;
        session_config.context_window_tokens = context_window_tokens;
        session_config.max_iterations = self.config.max_iterations;
        session_config.auto_compact = self.config.auto_compact;
        session_config.compaction_trigger_bytes = self.config.compaction_trigger_bytes;
        session_config.compaction_keep_tail = self.config.compaction_keep_tail;
        // A long-lived API can admit more than one request. Never stage/commit through the shared
        // repository index from those sessions; explicit file mutations remain sandbox-gated.
        session_config.auto_commit = false;

        let mut session = Session::new(session_config);
        let session_id = session.id;
        let metadata = SessionMeta::new(
            session_id,
            session.config.workspace_root.clone(),
            session.config.model.clone(),
            &request.prompt,
        )
        .with_effort(session.config.effort);
        let rollout = metadata
            .create_rollout(&self.config.sessions_dir, session_id)
            .await
            .map_err(|error| RunError::Storage(error.to_string()))?;

        let cancellation = TurnCancellation::new();
        // Agent events go straight into the bounded API queue. Project-capability setup and MCP
        // accounting still expose the core's legacy unbounded callback shape, so a bridge drains
        // that separate low-volume channel into the same bounded queue.
        // Crucially, model deltas and tool events can no longer accumulate behind a slow client.
        let (core_tx, mut core_rx) = mpsc::unbounded_channel();
        let (api_tx, api_rx) = mpsc::channel(self.config.event_queue_capacity);
        let forward_cancellation = cancellation.clone();
        let forward_api_tx = api_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = core_rx.recv().await {
                tokio::select! {
                    sent = forward_api_tx.send(event) => {
                        if sent.is_err() {
                            forward_cancellation.cancel();
                            break;
                        }
                    }
                    () = forward_cancellation.cancelled() => break,
                }
            }
        });

        // This is the first event and the queue is empty; awaiting it cannot depend on an HTTP
        // consumer that has not received `StartedPrompt` yet.
        api_tx
            .send(EventMsg::SessionConfigured { session_id })
            .await
            .map_err(|_| {
                RunError::Configuration("server event queue closed during setup".into())
            })?;
        let mut registry = ToolRegistry::with_builtins();
        self.register_project_capabilities(request.plan, &mut registry, &core_tx)
            .await;
        let approver = Arc::new(AutoApprover::new(self.config.allow.clone()));
        let (agent, event_queue_status) = Agent::new_bounded(
            self.config.client.clone(),
            registry,
            default_runner(),
            approver,
            api_tx,
            cancellation.clone(),
        );
        let task_cancellation = cancellation.clone();
        let timeout_cancellation = cancellation.clone();
        let (completion_tx, completion) = oneshot::channel();
        let turn_timeout = self.config.turn_timeout;
        let prompt = request.prompt;
        let plan = request.plan;
        tokio::spawn(async move {
            // Dropping this sender is the completion signal. Keep it through cooperative cleanup.
            let _completion_tx = completion_tx;
            // Retain the workspace permit until cancellation/timeout cleanup finishes.
            let _workspace_permit = workspace_permit;
            let mut rollout = Some(rollout);
            let turn = async {
                if plan {
                    agent
                        .run_plan_turn_cancellable(
                            &mut session,
                            &prompt,
                            &mut rollout,
                            &task_cancellation,
                        )
                        .await
                } else {
                    agent
                        .run_turn_cancellable(
                            &mut session,
                            &prompt,
                            &mut rollout,
                            &task_cancellation,
                        )
                        .await
                }
            };
            tokio::pin!(turn);
            tokio::select! {
                _ = &mut turn => {}
                () = tokio::time::sleep(turn_timeout) => {
                    timeout_cancellation.cancel();
                    let _ = core_tx.send(EventMsg::Error {
                        message: "server turn exceeded its time limit".to_string(),
                        recoverable: false,
                    });
                    // Cancellation is cooperative: wait for any already-running host mutation to
                    // finish safely instead of dropping the future at an arbitrary await point.
                    let _ = turn.await;
                }
            }
        });

        Ok(StartedPrompt {
            session_id,
            events: api_rx,
            cancellation,
            event_queue_status: Some(event_queue_status),
            completion,
        })
    }
}

/// Errors creating or running the local service.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("invalid server configuration: {0}")]
    InvalidConfiguration(String),
    #[error("could not obtain secure random bytes: {0}")]
    Entropy(String),
    #[error("server I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Prompt startup errors. HTTP responses intentionally do not reveal these internal details.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("invalid prompt: {0}")]
    InvalidPrompt(String),
    #[error("runtime configuration is invalid: {0}")]
    Configuration(String),
    #[error("session storage failed: {0}")]
    Storage(String),
}

#[derive(Clone)]
struct AppState {
    runner: Arc<dyn PromptRunner>,
    auth: ServerAuth,
    sessions_dir: PathBuf,
    permits: Arc<Semaphore>,
    limits: ApiLimits,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppState")
            .field("auth", &self.auth)
            .field("sessions_dir", &self.sessions_dir)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// Build the API router. No permissive CORS layer is installed.
pub fn router(
    runner: Arc<dyn PromptRunner>,
    auth: ServerAuth,
    sessions_dir: PathBuf,
    limits: ApiLimits,
) -> Result<Router, ServerError> {
    limits.validate()?;
    let body_limit = limits.max_request_body_bytes;
    let permits = Arc::new(Semaphore::new(limits.max_concurrent_prompts));
    let state = AppState {
        runner,
        auth,
        sessions_dir,
        permits,
        limits,
    };
    Ok(Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/{session_id}", get(get_session))
        .route("/v1/prompts", post(post_prompt))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(state))
}

/// Serve a prepared router until the supplied shutdown future resolves.
pub async fn serve_until<F>(
    listener: TcpListener,
    app: Router,
    shutdown: F,
) -> Result<(), ServerError>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(ServerError::Io)
}

#[derive(Debug, Serialize)]
struct Health<'a> {
    status: &'a str,
    service: &'a str,
    version: &'a str,
}

async fn health() -> Response {
    no_store_json(Json(Health {
        status: "ok",
        service: "grokforge",
        version: env!("CARGO_PKG_VERSION"),
    }))
}

async fn openapi() -> Response {
    let mut response = OPENAPI_JSON.into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Debug, Serialize)]
struct SessionSummary {
    session_id: String,
    title: Option<String>,
    parent_session_id: Option<String>,
    workspace: String,
    model: String,
    effort: Option<grokforge_core::PersistedEffort>,
    created_unix: i64,
    created_unix_nanos: u32,
    first_prompt: String,
}

impl From<SessionMeta> for SessionSummary {
    fn from(meta: SessionMeta) -> Self {
        Self {
            session_id: meta.session_id,
            title: meta.title,
            parent_session_id: meta.parent_session_id,
            workspace: meta.workspace.to_string_lossy().into_owned(),
            model: meta.model,
            effort: meta.effort,
            created_unix: meta.created_unix,
            created_unix_nanos: meta.created_unix_nanos,
            first_prompt: meta.first_prompt,
        }
    }
}

async fn list_sessions(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.permits(&headers) {
        return unauthorized();
    }
    let sessions = SessionMeta::list(&state.sessions_dir)
        .await
        .into_iter()
        .take(MAX_SESSION_RESULTS)
        .map(SessionSummary::from)
        .collect::<Vec<_>>();
    no_store_json(Json(serde_json::json!({ "sessions": sessions })))
}

async fn get_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    if !state.auth.permits(&headers) {
        return unauthorized();
    }
    if session_id.len() > MAX_SESSION_ID_BYTES || SessionId::parse_str(&session_id).is_err() {
        return json_error(StatusCode::NOT_FOUND, "session not found");
    }
    let found = SessionMeta::list(&state.sessions_dir)
        .await
        .into_iter()
        .find(|meta| meta.session_id == session_id);
    match found {
        Some(meta) => no_store_json(Json(SessionSummary::from(meta))),
        None => json_error(StatusCode::NOT_FOUND, "session not found"),
    }
}

async fn post_prompt(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !state.auth.permits(&headers) {
        return unauthorized();
    }
    let Ok(request) = serde_json::from_slice::<PromptRequest>(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid JSON prompt request");
    };
    if let Err(error) = request.validate() {
        return json_error(StatusCode::BAD_REQUEST, &error.to_string());
    }

    let Ok(permit) = Arc::clone(&state.permits).try_acquire_owned() else {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "all prompt slots are currently occupied",
        );
    };
    let started = match state.runner.start(request).await {
        Ok(started) => started,
        Err(RunError::InvalidPrompt(message)) => {
            return json_error(StatusCode::BAD_REQUEST, &message);
        }
        Err(error) => {
            tracing::error!(%error, "could not start local API prompt");
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "prompt could not be started",
            );
        }
    };

    let session_id = started.session_id.as_uuid().to_string();
    let stream = EventStream::new(started, permit, &state.limits);
    let mut response = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&session_id) {
        response
            .headers_mut()
            .insert("x-grokforge-session-id", value);
    }
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

struct EventStream {
    receiver: mpsc::Receiver<EventMsg>,
    cancellation: TurnCancellation,
    event_queue_status: Option<BoundedEventQueueStatus>,
    deadline: Pin<Box<tokio::time::Sleep>>,
    max_events: usize,
    max_bytes: usize,
    max_event_bytes: usize,
    events: usize,
    bytes: usize,
    done: bool,
}

impl EventStream {
    fn new(
        started: StartedPrompt,
        permit: tokio::sync::OwnedSemaphorePermit,
        limits: &ApiLimits,
    ) -> Self {
        let StartedPrompt {
            events,
            cancellation,
            event_queue_status,
            completion,
            ..
        } = started;
        // Admission bounds running turns, not merely attached HTTP bodies. A disconnected client
        // requests cancellation, while this lease remains held until the runner reports that any
        // in-flight host mutation has finished and the turn task has actually exited.
        tokio::spawn(async move {
            let _permit = permit;
            let _ = completion.await;
        });
        Self {
            receiver: events,
            cancellation,
            event_queue_status,
            deadline: Box::pin(tokio::time::sleep(limits.max_request_duration)),
            max_events: limits.max_stream_events,
            max_bytes: limits.max_stream_bytes,
            max_event_bytes: limits.max_event_bytes,
            events: 0,
            bytes: 0,
            done: false,
        }
    }

    fn terminal_error(&mut self, message: &str) -> Poll<Option<Result<Event, Infallible>>> {
        self.done = true;
        self.cancellation.cancel();
        let encoded = serde_json::to_string(&EventMsg::Error {
            message: message.to_string(),
            recoverable: false,
        })
        .unwrap_or_else(|_| {
            "{\"type\":\"error\",\"message\":\"stream closed\",\"recoverable\":false}".to_string()
        });
        Poll::Ready(Some(Ok(Event::default().event("grokforge").data(encoded))))
    }
}

impl Stream for EventStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        if self
            .event_queue_status
            .as_ref()
            .is_some_and(BoundedEventQueueStatus::overflowed)
        {
            return self.terminal_error("server event queue limit reached");
        }
        if self.deadline.as_mut().poll(context).is_ready() {
            return self.terminal_error("server request duration limit reached");
        }
        match Pin::new(&mut self.receiver).poll_recv(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(event)) => {
                let Ok(encoded) = serde_json::to_string(&event) else {
                    return self.terminal_error("event serialization failed");
                };
                let next_events = self.events.saturating_add(1);
                let next_bytes = self.bytes.saturating_add(encoded.len());
                if encoded.len() > self.max_event_bytes
                    || next_events > self.max_events
                    || next_bytes > self.max_bytes
                {
                    return self.terminal_error("server response limit reached");
                }
                self.events = next_events;
                self.bytes = next_bytes;
                Poll::Ready(Some(Ok(Event::default().event("grokforge").data(encoded))))
            }
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        if !self.done {
            self.cancellation.cancel();
        }
    }
}

fn unauthorized() -> Response {
    let mut response = json_error(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"grokforge\""),
    );
    response
}

fn json_error(status: StatusCode, message: &str) -> Response {
    let mut response = (status, Json(serde_json::json!({ "error": message }))).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn no_store_json<T: Serialize>(json: Json<T>) -> Response {
    let mut response = json.into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Parse and bind a socket address before starting the service. Kept here so the caller does not
/// need to depend on the HTTP implementation crate directly.
pub async fn bind(address: SocketAddr) -> Result<TcpListener, ServerError> {
    if !address.ip().is_loopback() {
        return Err(ServerError::InvalidConfiguration(
            "the local API is loopback-only; use a TLS reverse proxy for remote access".to_string(),
        ));
    }
    TcpListener::bind(address).await.map_err(ServerError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_loopback_is_rejected_even_with_an_explicit_token() {
        let address: IpAddr = "0.0.0.0".parse().expect("IP");
        assert!(resolve_auth(address, None).is_err());
        assert!(
            resolve_auth(
                address,
                Some("63hNf7kPq4Ws8Ty2Za5Vc9Bm1Dx6Lu0R".to_string())
            )
            .is_err()
        );
    }

    #[test]
    fn weak_tokens_are_rejected_and_digests_compare() {
        assert!(ServerAuth::new("1234").is_err());
        assert!(ServerAuth::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        let token = "63hNf7kPq4Ws8Ty2Za5Vc9Bm1Dx6Lu0R";
        let auth = ServerAuth::new(token).expect("strong token");
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("header"),
        );
        assert!(auth.permits(&headers));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong-but-long-token-value-1234567890"),
        );
        assert!(!auth.permits(&headers));
    }

    #[test]
    fn checked_in_openapi_is_valid_json() {
        let value: serde_json::Value = serde_json::from_str(OPENAPI_JSON).expect("OpenAPI JSON");
        assert_eq!(value["openapi"], "3.1.0");
        assert!(value["paths"]["/v1/prompts"].is_object());
    }

    #[test]
    fn plan_model_settings_are_isolated_from_execute_settings() {
        let workspace = tempfile::tempdir().expect("workspace");
        let storage = tempfile::tempdir().expect("storage");
        let client = XaiClient::new("http://127.0.0.1:9", "test-key").expect("client");
        let mut config = ProductionConfig::new(
            workspace.path().to_path_buf(),
            storage.path().to_path_buf(),
            client,
            "execute-model".to_string(),
        );
        config.effort = Some(Effort::Low);
        config.context_window_tokens = Some(64_000);
        config.plan_model = "plan-model".to_string();
        config.plan_context_window_tokens = Some(256_000);
        let runner = ProductionRunner::new(config).expect("runner");

        assert_eq!(
            runner.model_settings(false),
            ("execute-model", Some(Effort::Low), Some(64_000))
        );
        assert_eq!(
            runner.model_settings(true),
            ("plan-model", Some(Effort::High), Some(256_000))
        );
    }

    #[test]
    fn invalid_plan_model_is_rejected_with_the_execute_model_unchanged() {
        let workspace = tempfile::tempdir().expect("workspace");
        let storage = tempfile::tempdir().expect("storage");
        let client = XaiClient::new("http://127.0.0.1:9", "test-key").expect("client");
        let mut config = ProductionConfig::new(
            workspace.path().to_path_buf(),
            storage.path().to_path_buf(),
            client,
            "execute-model".to_string(),
        );
        config.plan_model = "invalid plan model".to_string();

        assert!(ProductionRunner::new(config).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plan_registry_never_starts_trusted_project_executables() {
        let workspace = tempfile::tempdir().expect("workspace");
        let storage = tempfile::tempdir().expect("session storage");
        let project = workspace.path().join(".grokforge");
        std::fs::create_dir(&project).expect("project config directory");
        let marker = workspace.path().join("mcp-started");
        let project_config = serde_json::json!({
            "servers": {
                "side-effect": {
                    "command": "/usr/bin/touch",
                    "args": [marker.to_string_lossy()]
                }
            }
        });
        std::fs::write(
            project.join("mcp.json"),
            serde_json::to_vec(&project_config).expect("MCP config"),
        )
        .expect("write MCP config");

        let client = XaiClient::new("http://127.0.0.1:9", "test-key").expect("client");
        let mut config = ProductionConfig::new(
            workspace.path().to_path_buf(),
            storage.path().to_path_buf(),
            client,
            "grok-build-0.1".to_string(),
        );
        config.trust_project_mcp = true;
        config.trust_project_tools = true;
        let runner = ProductionRunner::new(config).expect("runner");
        let (events, _events_rx) = mpsc::unbounded_channel();
        let mut registry = ToolRegistry::with_builtins();
        let before = registry.specs().len();

        runner
            .register_project_capabilities(true, &mut registry, &events)
            .await;

        assert_eq!(registry.specs().len(), before);
        assert!(!marker.exists(), "plan mode started a trusted MCP process");
    }

    #[tokio::test]
    async fn production_agent_queue_overflow_is_bounded_and_cancels_before_egress() {
        let workspace = tempfile::tempdir().expect("workspace");
        let storage = tempfile::tempdir().expect("session storage");
        let client = XaiClient::new("http://127.0.0.1:9", "test-key").expect("client");
        let mut config = ProductionConfig::new(
            workspace.path().to_path_buf(),
            storage.path().to_path_buf(),
            client,
            "grok-build-0.1".to_string(),
        );
        // SessionConfigured occupies the single slot. Because this test deliberately does not
        // drain the receiver, TurnStarted must refuse to grow a hidden backlog and cancel.
        config.event_queue_capacity = 1;
        let runner = ProductionRunner::new(config).expect("runner");
        let started = runner
            .start(PromptRequest {
                prompt: "this must not reach the provider".to_string(),
                plan: false,
            })
            .await
            .expect("started prompt");
        let StartedPrompt {
            cancellation,
            event_queue_status,
            completion,
            ..
        } = started;

        let _ = tokio::time::timeout(Duration::from_secs(1), completion)
            .await
            .expect("overflowed turn should finish cooperatively");
        assert!(cancellation.is_cancelled());
        assert!(
            event_queue_status
                .as_ref()
                .is_some_and(BoundedEventQueueStatus::overflowed)
        );
    }
}
