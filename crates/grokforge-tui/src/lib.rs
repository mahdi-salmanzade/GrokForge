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
use grokforge_core::{Agent, Session, SessionConfig, ToolRegistry};
use grokforge_sandbox::default_runner;
use grokforge_xai::XaiClient;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

pub use app::App;
pub use approver::{ChannelApprover, PendingApproval};

/// Launch the interactive TUI for a session. Returns when the user quits.
pub async fn run(
    client: XaiClient,
    config: SessionConfig,
    status_preset: String,
) -> io::Result<()> {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (approver, approvals_rx) = ChannelApprover::new();

    let model = config.model.clone();
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
    let session = Session::new(config);

    let mut app = App::new(
        agent,
        session,
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
