//! Launches the interactive TUI (the default, no-subcommand invocation).

use std::process::ExitCode;

use grokforge_core::SessionConfig;
use grokforge_protocol::{ApprovalPolicy, SandboxMode};
use grokforge_xai::XaiClient;

pub async fn launch() -> ExitCode {
    let Ok(api_key) = std::env::var("XAI_API_KEY") else {
        eprintln!("XAI_API_KEY is not set. Export it, or run `grokforge login` (lands in M8).");
        return ExitCode::from(3);
    };
    let base_url = std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai".to_string());

    let client = match XaiClient::new(&base_url, api_key) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client error: {e}");
            return ExitCode::from(2);
        }
    };

    let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let config = SessionConfig::new(workspace, "grok-build-0.1")
        // Default `auto` preset: workspace-write, ask before exceeding the sandbox.
        .with_policy(ApprovalPolicy::OnRequest, SandboxMode::WorkspaceWrite);

    match grokforge_tui::run(client, config, "auto".to_string()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tui error: {e}");
            ExitCode::from(1)
        }
    }
}
