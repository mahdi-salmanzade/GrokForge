//! `grokforge-tui` — the interactive terminal frontend.
//!
//! [`run`] sets up the terminal, builds the agent with an interactive approver, and drives the
//! [`app::App`] event loop. It restores the terminal on exit even if the app errors.

mod app;
mod approver;

use std::io::{self, Stdout};
use std::sync::Arc;

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
pub async fn run(client: XaiClient, config: SessionConfig, status_preset: String) -> io::Result<()> {
    run_session(client, Session::new(config), status_preset).await
}

/// Launch the interactive TUI for a (possibly resumed) session. Creates a rollout writer and
/// records session metadata so the session appears in `grokforge sessions`.
pub async fn run_session(
    client: XaiClient,
    session: Session,
    status_preset: String,
) -> io::Result<()> {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (approver, approvals_rx) = ChannelApprover::new();

    let model = session.config.model.clone();
    let workspace = session.config.workspace_root.clone();
    let agent = Arc::new(
        Agent::new(
            client,
            ToolRegistry::with_builtins(),
            default_runner(),
            Arc::new(approver),
            events_tx,
        )
        .interactive(),
    );

    let dir = sessions_dir();
    let rollout = RolloutWriter::create(&dir, session.id).await.ok();
    let meta = SessionMeta::new(session.id, workspace, model.clone(), "");
    let _ = meta.write(&dir, session.id).await;

    let mut app = App::new(
        agent,
        session,
        rollout,
        events_rx,
        approvals_rx,
        model,
        status_preset,
    );

    let mut terminal = setup_terminal()?;
    let result = app.run(&mut terminal).await;
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
