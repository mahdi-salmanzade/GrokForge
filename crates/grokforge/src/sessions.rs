//! `grokforge sessions` (list) and `grokforge resume` (reopen a persisted session in the TUI).

use std::process::ExitCode;

use grokforge_core::{RolloutWriter, Session, SessionConfig, SessionMeta, sessions_dir};
use grokforge_protocol::{ApprovalPolicy, SandboxMode};
use grokforge_xai::XaiClient;

/// Print the list of saved sessions.
#[allow(clippy::print_literal)] // static column headers
pub async fn list() -> ExitCode {
    let dir = sessions_dir();
    let metas = SessionMeta::list(&dir).await;
    if metas.is_empty() {
        println!("no saved sessions (looked in {})", dir.display());
        return ExitCode::SUCCESS;
    }
    println!("{:<10}  {:<20}  {:<16}  {}", "ID", "MODEL", "WORKSPACE", "FIRST PROMPT");
    for m in metas {
        let short_id: String = m.session_id.chars().take(8).collect();
        let workspace = m.workspace.file_name().map_or_else(
            || m.workspace.to_string_lossy().into_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        let prompt = if m.first_prompt.is_empty() {
            "(interactive)".to_string()
        } else {
            m.first_prompt.clone()
        };
        println!("{short_id:<10}  {:<20}  {:<16}  {prompt}", m.model, workspace);
    }
    ExitCode::SUCCESS
}

/// Resume a session: load its transcript and reopen the TUI continuing from it.
pub async fn resume(id: Option<String>) -> ExitCode {
    let dir = sessions_dir();
    let metas = SessionMeta::list(&dir).await;
    let Some(meta) = pick(&metas, id.as_deref()) else {
        eprintln!("no matching session found (see `grokforge sessions`)");
        return ExitCode::from(2);
    };

    let items = match RolloutWriter::read_all(&meta.rollout(&dir)).await {
        Ok(items) => items,
        Err(e) => {
            eprintln!("could not read rollout: {e}");
            return ExitCode::from(2);
        }
    };

    let Ok(api_key) = std::env::var("XAI_API_KEY") else {
        eprintln!("XAI_API_KEY is not set");
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

    let config = SessionConfig::new(meta.workspace.clone(), meta.model.clone())
        .with_policy(ApprovalPolicy::OnRequest, SandboxMode::WorkspaceWrite);
    let session = Session::with_history(config, items);

    eprintln!(
        "resuming session {} ({} items)",
        &meta.session_id[..8.min(meta.session_id.len())],
        session.history.len()
    );
    match grokforge_tui::run_session(client, session, "auto".to_string()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tui error: {e}");
            ExitCode::from(1)
        }
    }
}

/// Choose a session by id prefix, or the most recent one.
fn pick<'a>(metas: &'a [SessionMeta], id: Option<&str>) -> Option<&'a SessionMeta> {
    match id {
        Some(prefix) => metas.iter().find(|m| m.session_id.starts_with(prefix)),
        None => metas.first(),
    }
}
