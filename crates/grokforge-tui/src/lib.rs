//! `grokforge-tui` — the interactive terminal frontend.
//!
//! [`run`] sets up the terminal, builds the agent with an interactive approver, and drives the
//! [`app::App`] event loop. It restores the terminal on exit even if the app errors.

mod app;
mod approver;
mod brand;

use std::io::{self, Stdout};
use std::sync::Arc;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use grokforge_core::{
    Agent, RolloutWriter, Session, SessionConfig, SessionMeta, ToolRegistry, sessions_dir,
};
use grokforge_sandbox::default_runner;
use grokforge_xai::XaiClient;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

pub use app::App;
pub use approver::{ChannelApprover, PendingApproval};

/// Launch the interactive TUI for a fresh session.
pub async fn run(
    client: XaiClient,
    config: SessionConfig,
    status_preset: String,
    trust_project_mcp: bool,
) -> io::Result<()> {
    run_session(
        client,
        Session::new(config),
        status_preset,
        trust_project_mcp,
    )
    .await
}

/// Launch the interactive TUI for a (possibly resumed) session. Creates a rollout writer and
/// records session metadata so the session appears in `grokforge sessions`.
pub async fn run_session(
    client: XaiClient,
    mut session: Session,
    status_preset: String,
    trust_project_mcp: bool,
) -> io::Result<()> {
    // Acquire the session's lifetime lock and refresh persisted history before repository checks,
    // MCP process startup, or any other preflight side effect. A second resume fails here.
    let dir = sessions_dir()?;
    let metadata_path = dir.join(format!("rollout-{}.meta.json", session.id.as_uuid()));
    let metadata_exists = tokio::fs::try_exists(&metadata_path).await.unwrap_or(false);
    let rollout = match RolloutWriter::open_and_read(&dir, session.id).await {
        Ok((rollout, persisted_history)) => {
            if metadata_exists {
                session.history = persisted_history;
            }
            Some(rollout)
        }
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("could not open durable session transcript: {error}"),
            ));
        }
    };

    run_session_ready(
        client,
        session,
        rollout,
        status_preset,
        metadata_exists,
        dir,
        trust_project_mcp,
    )
    .await
}

/// Launch a session whose rollout has already been exclusively opened and atomically read.
/// Resume frontends use this so locking happens before workspace, Git, and API preflight.
pub async fn run_locked_session(
    client: XaiClient,
    session: Session,
    rollout: RolloutWriter,
    status_preset: String,
    trust_project_mcp: bool,
) -> io::Result<()> {
    let dir = sessions_dir()?;
    let metadata_path = dir.join(format!("rollout-{}.meta.json", session.id.as_uuid()));
    let metadata_exists = tokio::fs::try_exists(&metadata_path).await.unwrap_or(false);
    run_session_ready(
        client,
        session,
        Some(rollout),
        status_preset,
        metadata_exists,
        dir,
        trust_project_mcp,
    )
    .await
}

async fn run_session_ready(
    client: XaiClient,
    mut session: Session,
    rollout: Option<RolloutWriter>,
    status_preset: String,
    metadata_exists: bool,
    dir: std::path::PathBuf,
    trust_project_mcp: bool,
) -> io::Result<()> {
    let model = session.config.model.clone();
    let workspace = session.config.workspace_root.clone();

    // Metadata and rollout are the canonical recovery record. Finish both before Git inspection,
    // MCP process startup, or the first model request.
    if !metadata_exists {
        let meta = SessionMeta::new(session.id, workspace.clone(), model.clone(), "")
            .with_effort(session.config.effort);
        meta.write(&dir, session.id).await.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not persist session metadata: {error}"),
            )
        })?;
    }

    let auto_commit_warning = protect_user_changes(&mut session);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (approver, approvals_rx) = ChannelApprover::new();

    let mut registry = ToolRegistry::with_builtins();
    if trust_project_mcp {
        eprintln!("{}", grokforge_core::mcp_config::PROJECT_MCP_TRUST_WARNING);
        grokforge_core::mcp_config::connect_and_register_trusted(&workspace, &mut registry).await;
    } else {
        grokforge_core::mcp_config::connect_and_register(&workspace, &mut registry).await;
    }

    let agent = Arc::new(
        Agent::new(
            client,
            registry,
            default_runner(),
            Arc::new(approver),
            events_tx,
        )
        .interactive(),
    );

    let mut app = App::new(
        agent,
        session,
        rollout,
        events_rx,
        approvals_rx,
        model,
        status_preset,
    );
    if let Some(warning) = auto_commit_warning {
        app.set_startup_notice(warning);
    }

    let mut terminal = TerminalGuard::new(setup_terminal()?);
    let result = app.run(terminal.get_mut()).await;
    let restore = terminal.restore();
    match result {
        Err(error) => Err(error),
        Ok(()) => restore,
    }
}

fn protect_user_changes(session: &mut Session) -> Option<String> {
    if !session.config.auto_commit {
        return None;
    }
    // Foreground sessions share the user's working tree. Staging after a tool call would race any
    // user/editor write to the same path, so commit-based undo is intentionally unavailable until
    // the descriptor-safe foreground edit journal lands. Keep the runtime flag honest instead of
    // implying that a clean foreground repository will be auto-committed.
    if !session.config.isolated_worktree {
        session.config.auto_commit = false;
        return None;
    }
    let git = grokforge_git::Git::discover(&session.config.workspace_root)?;
    match git.is_dirty() {
        Ok(false) => None,
        Ok(true) => {
            session.config.auto_commit = false;
            Some("auto-commit disabled: workspace had pre-existing uncommitted changes".to_string())
        }
        Err(error) => {
            session.config.auto_commit = false;
            Some(format!(
                "auto-commit disabled: could not verify clean workspace ({error})"
            ))
        }
    }
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Capture the mouse so the scroll wheel belongs to GrokForge (it scrolls the transcript)
    // rather than falling through to the terminal, which would scroll its native scrollback and
    // expose the pre-launch shell history behind the alternate screen.
    if let Err(error) = execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    ) {
        let _ = execute!(
            stdout,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        return Err(error);
    }
    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => Ok(terminal),
        Err(error) => {
            let mut stdout = io::stdout();
            let _ = execute!(
                stdout,
                DisableMouseCapture,
                DisableBracketedPaste,
                LeaveAlternateScreen
            );
            let _ = disable_raw_mode();
            Err(error)
        }
    }
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    let mut first_error = None;
    if let Err(error) = terminal.show_cursor() {
        first_error = Some(error);
    }
    if let Err(error) = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    ) && first_error.is_none()
    {
        first_error = Some(error);
    }
    if let Err(error) = disable_raw_mode()
        && first_error.is_none()
    {
        first_error = Some(error);
    }
    first_error.map_or(Ok(()), Err)
}

#[derive(Debug)]
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    restored: bool,
}

impl TerminalGuard {
    fn new(terminal: Terminal<CrosstermBackend<Stdout>>) -> Self {
        Self {
            terminal,
            restored: false,
        }
    }

    fn get_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

    fn restore(&mut self) -> io::Result<()> {
        let result = restore_terminal(&mut self.terminal);
        self.restored = result.is_ok();
        result
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.restored {
            let _ = restore_terminal(&mut self.terminal);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::expect_used)]

    use std::process::Command;

    use super::*;

    #[test]
    fn foreground_workspace_disables_auto_commit_without_a_misleading_dirty_warning() {
        let dir = tempfile::tempdir().expect("workspace");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .expect("git init")
                .success()
        );
        std::fs::write(dir.path().join("user.txt"), "user change\n").expect("user change");
        let mut session = Session::new(SessionConfig::new(dir.path().to_path_buf(), "model"));
        let warning = protect_user_changes(&mut session);
        assert!(!session.config.auto_commit);
        assert!(warning.is_none());
    }

    #[test]
    fn dirty_isolated_workspace_disables_auto_commit_with_a_warning() {
        let dir = tempfile::tempdir().expect("workspace");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .expect("git init")
                .success()
        );
        std::fs::write(dir.path().join("user.txt"), "user change\n").expect("user change");
        let mut session = Session::new(SessionConfig::new(dir.path().to_path_buf(), "model"));
        session.config.isolated_worktree = true;

        let warning = protect_user_changes(&mut session);

        assert!(!session.config.auto_commit);
        assert!(warning.is_some());
    }
}
