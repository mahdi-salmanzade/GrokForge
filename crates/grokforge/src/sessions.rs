//! `grokforge sessions` (list) and `grokforge resume` (reopen a persisted session in the TUI).

use std::process::ExitCode;

use grokforge_core::{RolloutWriter, Session, SessionConfig, SessionMeta, sessions_dir};
use grokforge_protocol::{ApprovalPolicy, SandboxMode, SessionId};
use grokforge_xai::XaiClient;

/// Print the list of saved sessions.
#[allow(clippy::print_literal)] // static column headers
pub async fn list() -> ExitCode {
    let dir = match sessions_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("could not locate secure session storage: {error}");
            return ExitCode::from(2);
        }
    };
    let metas = SessionMeta::list(&dir).await;
    if metas.is_empty() {
        println!("no saved sessions (looked in {})", dir.display());
        return ExitCode::SUCCESS;
    }
    println!(
        "{:<10}  {:<20}  {:<16}  {}",
        "ID", "MODEL", "WORKSPACE", "FIRST PROMPT"
    );
    for m in metas {
        let short_id: String = m.session_id.chars().take(8).collect();
        let workspace = m.workspace.file_name().map_or_else(
            || m.workspace.to_string_lossy().into_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        let prompt = if m.first_prompt.is_empty() {
            "(interactive)".to_string()
        } else {
            crate::sanitize_terminal_line(&m.first_prompt)
        };
        let model = crate::sanitize_terminal_line(&m.model);
        let workspace = crate::sanitize_terminal_line(&workspace);
        println!("{short_id:<10}  {model:<20}  {workspace:<16}  {prompt}");
    }
    ExitCode::SUCCESS
}

/// Resume a session: load its transcript and reopen the TUI continuing from it.
#[allow(clippy::too_many_lines)]
pub async fn resume(id: Option<String>, trust_project_mcp: bool) -> ExitCode {
    let dir = match sessions_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("could not locate secure session storage: {error}");
            return ExitCode::from(2);
        }
    };
    let mut metas = SessionMeta::list(&dir).await;
    if id.is_none() {
        let current = match std::env::current_dir().and_then(std::fs::canonicalize) {
            Ok(workspace) => workspace,
            Err(error) => {
                eprintln!("could not determine current workspace: {error}");
                return ExitCode::from(2);
            }
        };
        let workspace = project_root(&current).unwrap_or(current);
        metas.retain(|meta| project_root(&meta.workspace).is_some_and(|path| path == workspace));
    }
    let meta = match pick(&metas, id.as_deref()) {
        Ok(meta) => meta,
        Err(PickError::NoMatch) => {
            eprintln!("no matching session found (see `grokforge sessions`)");
            return ExitCode::from(2);
        }
        Err(PickError::EmptyPrefix) => {
            eprintln!("session id prefix must not be empty");
            return ExitCode::from(2);
        }
        Err(PickError::Ambiguous) => {
            eprintln!("session id prefix is ambiguous; provide more characters");
            return ExitCode::from(2);
        }
    };

    let session_id = match SessionId::parse_str(&meta.session_id) {
        Ok(session_id) => session_id,
        Err(error) => {
            eprintln!("invalid persisted session id: {error}");
            return ExitCode::from(2);
        }
    };
    // Take the lifetime lock and atomically repair/read the rollout before any workspace Git
    // inspection, model validation, or external-process setup.
    let (rollout, items) = match RolloutWriter::open_and_read(&dir, session_id).await {
        Ok(opened) => opened,
        Err(error) => {
            eprintln!("could not exclusively resume session: {error}");
            return ExitCode::from(2);
        }
    };

    let workspace = match std::fs::canonicalize(&meta.workspace) {
        Ok(workspace) if workspace.is_dir() => workspace,
        Ok(_) => {
            eprintln!("saved workspace is not a directory");
            return ExitCode::from(2);
        }
        Err(error) => {
            eprintln!("could not resolve saved workspace: {error}");
            return ExitCode::from(2);
        }
    };
    let current_identity = grokforge_git::Git::discover(&workspace)
        .and_then(|git| std::fs::canonicalize(git.root()).ok())
        .unwrap_or_else(|| workspace.clone());
    if let Some(expected) = &meta.workspace_identity {
        // `expected` was canonicalized when metadata was written. Do not canonicalize it again:
        // doing so would let a later symlink replacement rewrite the expected identity too.
        if expected != &current_identity {
            eprintln!("saved workspace no longer matches the recorded project identity");
            return ExitCode::from(2);
        }
    }
    if !meta.fingerprint_matches(&workspace) {
        eprintln!("saved workspace was replaced or no longer matches its Git metadata");
        return ExitCode::from(2);
    }

    let Some(api_key) = crate::credentials::resolve(false).await else {
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
    if let Err(code) = crate::validate_model_startup(&client, &meta.model).await {
        return code;
    }

    let config = SessionConfig::new(workspace, meta.model.clone())
        .with_policy(ApprovalPolicy::OnRequest, SandboxMode::WorkspaceWrite);
    let session = match Session::with_id_and_history(config, &meta.session_id, items) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("invalid persisted session id: {error}");
            return ExitCode::from(2);
        }
    };

    eprintln!(
        "resuming session {} ({} items)",
        &meta.session_id[..8.min(meta.session_id.len())],
        session.history.len()
    );
    match grokforge_tui::run_locked_session(
        client,
        session,
        rollout,
        "auto".to_string(),
        trust_project_mcp,
    )
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tui error: {e}");
            ExitCode::from(1)
        }
    }
}

/// Choose a session by id prefix, or the most recent one.
fn pick<'a>(metas: &'a [SessionMeta], id: Option<&str>) -> Result<&'a SessionMeta, PickError> {
    match id {
        Some(prefix) if prefix.trim().is_empty() => Err(PickError::EmptyPrefix),
        Some(prefix) => {
            let mut matches = metas
                .iter()
                .filter(|meta| meta.session_id.starts_with(prefix));
            let first = matches.next().ok_or(PickError::NoMatch)?;
            if matches.next().is_some() {
                Err(PickError::Ambiguous)
            } else {
                Ok(first)
            }
        }
        None => metas.first().ok_or(PickError::NoMatch),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickError {
    NoMatch,
    EmptyPrefix,
    Ambiguous,
}

fn project_root(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    Some(grokforge_git::Git::discover(&canonical).map_or(canonical, |git| git.root().to_path_buf()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            session_id: id.to_string(),
            workspace: PathBuf::from("/workspace"),
            workspace_identity: None,
            workspace_fingerprint: None,
            model: "model".to_string(),
            created_unix: 0,
            created_unix_nanos: 0,
            first_prompt: String::new(),
        }
    }

    #[test]
    fn pick_rejects_empty_and_ambiguous_prefixes() {
        let metas = vec![meta("abcd-1"), meta("abcd-2")];
        assert!(matches!(
            pick(&metas, Some("")),
            Err(PickError::EmptyPrefix)
        ));
        assert!(matches!(
            pick(&metas, Some("abcd")),
            Err(PickError::Ambiguous)
        ));
        assert!(matches!(
            pick(&metas, Some("missing")),
            Err(PickError::NoMatch)
        ));
        assert_eq!(
            pick(&metas, Some("abcd-1")).map(|m| m.session_id.as_str()),
            Ok("abcd-1")
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_root_groups_subdirectories_but_not_other_repositories() {
        use std::process::Command;

        let repo = tempfile::tempdir().expect("repo");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo.path())
                .status()
                .expect("git init")
                .success()
        );
        let nested = repo.path().join("nested");
        std::fs::create_dir(&nested).expect("nested");
        let other = tempfile::tempdir().expect("other");
        assert_eq!(project_root(&nested), project_root(repo.path()));
        assert_ne!(project_root(&nested), project_root(other.path()));
    }
}
