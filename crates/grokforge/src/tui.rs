//! Launches the interactive TUI (the default, no-subcommand invocation).

use std::process::ExitCode;

use grokforge_core::SessionConfig;
use grokforge_protocol::{ApprovalPolicy, SandboxMode};
use grokforge_xai::XaiClient;

pub async fn launch(trust_project_mcp: bool) -> ExitCode {
    let workspace = match canonical_workspace() {
        Ok(workspace) => workspace,
        Err(error) => {
            eprintln!("cannot start TUI: {error}");
            return ExitCode::from(2);
        }
    };
    // Interactive: resolve env → keychain → hidden prompt (and save to keychain).
    let Some(api_key) = crate::credentials::resolve(true).await else {
        return ExitCode::from(3);
    };
    let base_url = std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai".to_string());

    let client = match XaiClient::new(&base_url, api_key) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client error: {}", crate::sanitize_terminal(&e.to_string()));
            return ExitCode::from(2);
        }
    };
    let model = "grok-build-0.1";
    if let Err(code) = crate::validate_model_startup(&client, model).await {
        return code;
    }

    let config = SessionConfig::new(workspace, model)
        // Default `auto` preset: workspace-write, ask before exceeding the sandbox.
        .with_policy(ApprovalPolicy::OnRequest, SandboxMode::WorkspaceWrite);

    match grokforge_tui::run(client, config, "auto".to_string(), trust_project_mcp).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tui error: {e}");
            ExitCode::from(1)
        }
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
