//! Launches the interactive TUI (the default, no-subcommand invocation).

use std::process::ExitCode;

use grokforge_core::SessionConfig;
use grokforge_protocol::{ApprovalPolicy, SandboxMode};
use grokforge_xai::{Effort, XaiClient, model_supports_effort};

pub async fn launch(
    trust_project_mcp: bool,
    trust_project_config: bool,
    model_override: Option<String>,
    effort_override: Option<String>,
) -> ExitCode {
    let workspace = match canonical_workspace() {
        Ok(workspace) => workspace,
        Err(error) => {
            eprintln!("cannot start TUI: {}", crate::sanitize_terminal(&error));
            return ExitCode::from(2);
        }
    };
    let settings = match grokforge_config::Config::load_with_project_config(
        &workspace,
        trust_project_config,
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
    // Interactive: env override → password-unlock the encrypted file → first-run onboarding.
    let Some(api_key) = crate::credentials::resolve(true).await else {
        return ExitCode::from(3);
    };
    let base_url =
        std::env::var("XAI_BASE_URL").unwrap_or_else(|_| settings.provider.grok.base_url.clone());

    let client = match XaiClient::new(&base_url, api_key) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client error: {}", crate::sanitize_terminal(&e.to_string()));
            return ExitCode::from(2);
        }
    };
    let model = model_override.unwrap_or(settings.agent.default_model);
    let model_catalog = match crate::model_catalog_startup(&client, &model).await {
        Ok(models) => models,
        Err(code) => return code,
    };
    let selected_model = model_catalog.iter().find(|candidate| {
        candidate.id == model || candidate.aliases.iter().any(|alias| alias == &model)
    });
    let active_model = selected_model.map_or(model, |candidate| candidate.id.clone());
    let context_window_tokens = selected_model.and_then(|candidate| candidate.context_window);

    let mut config = SessionConfig::new(workspace, active_model)
        // Default `auto` preset: workspace-write, ask before exceeding the sandbox.
        .with_policy(ApprovalPolicy::OnRequest, SandboxMode::WorkspaceWrite);
    config.plan_model = settings.agent.plan_model.clone();
    // Best-effort: bound the request to the model's real context window so a long session cannot
    // exceed the prompt-token limit (falls back to a conservative default when unavailable).
    config.model_catalog = model_catalog;
    config.context_window_tokens = context_window_tokens;
    config.max_iterations = settings.agent.max_iterations;
    config.auto_compact = settings.agent.auto_compact;
    config.compaction_trigger_bytes = settings.agent.compaction_trigger_bytes;
    config.compaction_keep_tail = settings.agent.compaction_keep_tail;
    config.effort = match effort_override.as_deref() {
        Some("auto") => None,
        Some("low") => Some(Effort::Low),
        Some("medium") => Some(Effort::Medium),
        Some("high") => Some(Effort::High),
        Some("xhigh") => Some(Effort::Xhigh),
        Some(_) => {
            eprintln!("invalid --effort value");
            return ExitCode::from(2);
        }
        None => settings.agent.effort.map(configured_effort),
    };
    if config
        .effort
        .is_some_and(|effort| !model_supports_effort(&config.model, effort))
    {
        eprintln!("reasoning effort `xhigh` requires an xAI multi-agent model");
        return ExitCode::from(2);
    }

    match grokforge_tui::run(client, config, "auto".to_string(), trust_project_mcp).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tui error: {e}");
            ExitCode::from(1)
        }
    }
}

fn configured_effort(effort: grokforge_config::Effort) -> Effort {
    match effort {
        grokforge_config::Effort::Low => Effort::Low,
        grokforge_config::Effort::Medium => Effort::Medium,
        grokforge_config::Effort::High => Effort::High,
        grokforge_config::Effort::Xhigh => Effort::Xhigh,
    }
}

fn canonical_workspace() -> Result<std::path::PathBuf, String> {
    let current = std::env::current_dir()
        .map_err(|error| format!("current directory is unavailable: {error}"))?;
    let workspace = std::fs::canonicalize(&current)
        .map_err(|error| format!("cannot resolve {}: {error}", current.display()))?;
    if !workspace.is_dir() {
        return Err(format!("{} is not a directory", workspace.display()));
    }
    Ok(workspace)
}
