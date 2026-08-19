//! `grokforge serve`: bounded, authenticated local HTTP access to the real agent loop.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use grokforge_core::sessions_dir;
use grokforge_server::{ApiLimits, ProductionConfig, ProductionRunner};
use grokforge_xai::{Effort, ModelInfo, XaiClient, model_supports_effort};
use zeroize::Zeroize as _;

/// A CLI bearer token whose debug representation cannot reveal the secret.
#[derive(Clone)]
pub struct SecretToken(String);

impl SecretToken {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for SecretToken {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.to_string()))
    }
}

impl std::fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Drop for SecretToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // Each trust decision is an independent explicit opt-in.
pub struct ServeArgs {
    pub bind: SocketAddr,
    pub token: Option<SecretToken>,
    pub cd: Option<PathBuf>,
    pub preset: String,
    pub allow: Vec<String>,
    pub max_concurrency: usize,
    pub timeout_secs: u64,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub trust_project_mcp: bool,
    pub trust_project_config: bool,
    pub trust_project_tools: bool,
}

/// Resolve a configured model through an already-fetched catalog. Model discovery is allowed to
/// fail open at startup (the shared CLI policy), so an empty catalog preserves the configured slug
/// with no context-window hint. A non-empty catalog is authoritative and rejects retired slugs.
fn resolve_catalog_model(
    catalog: &[ModelInfo],
    requested: &str,
) -> Result<(String, Option<u64>), String> {
    if catalog.is_empty() {
        return Ok((requested.to_string(), None));
    }
    catalog
        .iter()
        .find(|candidate| {
            candidate.id == requested || candidate.aliases.iter().any(|alias| alias == requested)
        })
        .map(|candidate| (candidate.id.clone(), candidate.context_window))
        .ok_or_else(|| {
            let available = catalog
                .iter()
                .map(|candidate| candidate.id.as_str())
                .take(32)
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if catalog.len() > 32 { ", …" } else { "" };
            format!(
                "configured plan model `{requested}` is not advertised; available: {available}{suffix}"
            )
        })
}

#[allow(clippy::too_many_lines)] // Linear startup keeps every security decision visible in order.
pub async fn run(mut args: ServeArgs) -> ExitCode {
    if !args.bind.ip().is_loopback() {
        eprintln!(
            "grokforge serve is loopback-only; use a TLS reverse proxy in front of the loopback listener for remote access"
        );
        return ExitCode::from(2);
    }
    let Some((approval_policy, sandbox_mode)) = crate::headless::preset_policy(&args.preset) else {
        eprintln!("unknown --preset (readonly|auto|strict)");
        return ExitCode::from(2);
    };
    if args.preset == "yolo" {
        eprintln!("the persistent HTTP server does not support the yolo preset");
        return ExitCode::from(2);
    }

    // Validate authentication before unlocking provider credentials. A non-loopback typo must
    // not trigger a password prompt and then fail later.
    // Move the CLI wrapper out of `args` so its plaintext copy is zeroized immediately after the
    // authentication digest has been derived. A generated token must remain available until the
    // listener binds so it can be shown once; a configured token never needs to be retained.
    let configured_token = args.token.take().map(|token| token.expose().to_string());
    let mut auth = match grokforge_server::resolve_auth(args.bind.ip(), configured_token) {
        Ok(auth) => auth,
        Err(error) => {
            eprintln!("server authentication configuration error: {error}");
            return ExitCode::from(2);
        }
    };
    if !auth.generated {
        auth.token.zeroize();
    }

    let workspace = match crate::headless::resolve_workspace(args.cd.as_deref()) {
        Ok(workspace) => workspace,
        Err(error) => {
            eprintln!("invalid --cd: {error}");
            return ExitCode::from(2);
        }
    };
    let settings = match grokforge_config::Config::load_with_project_config(
        &workspace,
        args.trust_project_config,
    ) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!(
                "configuration error: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return ExitCode::from(2);
        }
    };
    let allow = match crate::headless::parse_allow(&args.allow, &workspace) {
        Ok(allow) => allow,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let effort = match args.effort.as_deref() {
        Some(value) => {
            let Ok(effort) = crate::headless::parse_effort(value) else {
                eprintln!("invalid --effort `{value}` (auto|low|medium|high|xhigh)");
                return ExitCode::from(2);
            };
            effort
        }
        None => settings
            .agent
            .effort
            .map(crate::headless::configured_effort),
    };
    let model = args
        .model
        .unwrap_or_else(|| settings.agent.default_model.clone());
    let Some(api_key) = crate::credentials::resolve(false).await else {
        return ExitCode::from(3);
    };
    let base_url =
        std::env::var("XAI_BASE_URL").unwrap_or_else(|_| settings.provider.grok.base_url.clone());
    let client = match XaiClient::new(&base_url, api_key) {
        Ok(client) => client,
        Err(error) => {
            eprintln!(
                "client error: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return ExitCode::from(2);
        }
    };
    let model_catalog = match crate::model_catalog_startup(&client, &model).await {
        Ok(models) => models,
        Err(code) => return code,
    };
    let selected_model = model_catalog.iter().find(|candidate| {
        candidate.id == model || candidate.aliases.iter().any(|alias| alias == &model)
    });
    let active_model = selected_model.map_or(model, |candidate| candidate.id.clone());
    if effort.is_some_and(|value| !model_supports_effort(&active_model, value)) {
        eprintln!("reasoning effort `xhigh` requires an xAI multi-agent model");
        return ExitCode::from(2);
    }
    let context_window_tokens = selected_model.and_then(|candidate| candidate.context_window);
    let (plan_model, plan_context_window_tokens) =
        match resolve_catalog_model(&model_catalog, &settings.agent.plan_model) {
            Ok(resolved) => resolved,
            Err(error) => {
                eprintln!(
                    "model validation failed: {}",
                    crate::sanitize_terminal(&error)
                );
                return ExitCode::from(3);
            }
        };
    if !model_supports_effort(&plan_model, Effort::High) {
        eprintln!("configured plan model does not support high reasoning effort");
        return ExitCode::from(2);
    }
    let session_storage = match sessions_dir() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("could not locate secure session storage: {error}");
            return ExitCode::from(2);
        }
    };
    let mcp_oauth_tokens = if args.trust_project_mcp {
        crate::credentials::mcp_access_tokens(&workspace).await
    } else {
        std::collections::BTreeMap::new()
    };

    let mut production =
        ProductionConfig::new(workspace, session_storage.clone(), client, active_model);
    production.effort = effort;
    production.context_window_tokens = context_window_tokens;
    production.plan_model = plan_model;
    production.plan_context_window_tokens = plan_context_window_tokens;
    production.approval_policy = approval_policy;
    production.sandbox_mode = sandbox_mode;
    production.allow = allow;
    production.max_iterations = settings.agent.max_iterations;
    production.auto_compact = settings.agent.auto_compact;
    production.compaction_trigger_bytes = settings.agent.compaction_trigger_bytes;
    production.compaction_keep_tail = settings.agent.compaction_keep_tail;
    production.trust_project_mcp = args.trust_project_mcp;
    production.mcp_oauth_tokens = mcp_oauth_tokens;
    production.trust_project_tools = args.trust_project_tools;
    production.turn_timeout = Duration::from_secs(args.timeout_secs);
    let runner = match ProductionRunner::new(production) {
        Ok(runner) => std::sync::Arc::new(runner),
        Err(error) => {
            eprintln!(
                "server runtime configuration error: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return ExitCode::from(2);
        }
    };
    let limits = ApiLimits {
        max_concurrent_prompts: args.max_concurrency,
        max_request_duration: Duration::from_secs(args.timeout_secs),
        ..ApiLimits::default()
    };
    let app = match grokforge_server::router(runner, auth.auth.clone(), session_storage, limits) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("could not configure local API: {error}");
            return ExitCode::from(2);
        }
    };
    let listener = match grokforge_server::bind(args.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("could not bind local API to {}: {error}", args.bind);
            return ExitCode::from(1);
        }
    };
    let address = listener.local_addr().unwrap_or(args.bind);
    eprintln!("GrokForge local API listening on http://{address}");
    if auth.generated {
        eprintln!("ephemeral bearer token (shown once): {}", auth.token);
    } else {
        eprintln!("bearer authentication: configured");
    }
    eprintln!("OpenAPI: http://{address}/openapi.json");
    // The router retains only the digest. Do not keep the printable token alive for the lifetime
    // of this persistent process.
    drop(auth);

    match grokforge_server::serve_until(listener, app, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("local API stopped: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_is_redacted() {
        let token: SecretToken = "this-is-a-test-secret-that-must-never-print-123"
            .parse()
            .expect("infallible token parse");
        let rendered = format!("{token:?}");
        assert_eq!(rendered, "[REDACTED]");
        assert!(!rendered.contains(token.expose()));
    }

    #[test]
    fn plan_model_resolution_uses_alias_context_and_unavailable_fallback() {
        let catalog = vec![ModelInfo {
            id: "grok-4.5".to_string(),
            created: None,
            owned_by: None,
            aliases: vec!["grok-plan-latest".to_string()],
            context_window: Some(500_000),
        }];
        assert_eq!(
            resolve_catalog_model(&catalog, "grok-plan-latest").unwrap(),
            ("grok-4.5".to_string(), Some(500_000))
        );
        assert!(resolve_catalog_model(&catalog, "retired-plan-model").is_err());
        assert_eq!(
            resolve_catalog_model(&[], "offline-plan-model").unwrap(),
            ("offline-plan-model".to_string(), None)
        );
    }
}
