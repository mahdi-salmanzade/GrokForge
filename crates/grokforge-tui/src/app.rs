//! The interactive TUI application: an async event loop over terminal input, agent events, and
//! approval requests, rendering a scrolling transcript, a composer, a status line, and an
//! approval modal.
//!
//! This is the first working cut. It uses the alternate screen for robustness; the inline
//! viewport + native-scrollback render pipeline (the codex-style differentiator in the design
//! docs) is the planned upgrade and slots in behind the same event/op flow.

use std::sync::Arc;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use grokforge_core::{Agent, Session};
use grokforge_protocol::{Decision, EventMsg};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::approver::PendingApproval;

/// A finalized transcript entry.
#[derive(Debug)]
enum Entry {
    User(String),
    Assistant(String),
    Tool(String),
    ToolResult { ok: bool, text: String },
    Git(String),
    Error(String),
    Info(String),
}

/// The running application state.
pub struct App {
    agent: Arc<Agent>,
    session: Option<Session>,
    transcript: Vec<Entry>,
    streaming: Option<String>,
    composer: String,
    scroll: u16,
    follow: bool,
    pending: Option<PendingApproval>,
    running: bool,
    should_quit: bool,
    status_model: String,
    status_preset: String,

    // The channel stays open for the app's lifetime because `agent` (Arc) holds the sender.
    events_rx: mpsc::UnboundedReceiver<EventMsg>,
    approvals_rx: mpsc::UnboundedReceiver<PendingApproval>,
    turn_handle: Option<JoinHandle<Session>>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("transcript_len", &self.transcript.len())
            .field("running", &self.running)
            .field("has_pending_approval", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

impl App {
    #[must_use]
    pub fn new(
        agent: Arc<Agent>,
        session: Session,
        events_rx: mpsc::UnboundedReceiver<EventMsg>,
        approvals_rx: mpsc::UnboundedReceiver<PendingApproval>,
        status_model: String,
        status_preset: String,
    ) -> Self {
        Self {
            agent,
            session: Some(session),
            transcript: vec![Entry::Info(
                "GrokForge — type a message and press Enter. Ctrl+C to quit.".to_string(),
            )],
            streaming: None,
            composer: String::new(),
            scroll: 0,
            follow: true,
            pending: None,
            running: false,
            should_quit: false,
            status_model,
            status_preset,
            events_rx,
            approvals_rx,
            turn_handle: None,
        }
    }

    /// Run the event loop until the user quits.
    pub async fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> std::io::Result<()> {
        let mut input = EventStream::new();
        while !self.should_quit {
            terminal.draw(|f| self.render(f))?;

            tokio::select! {
                maybe_key = input.next() => {
                    if let Some(Ok(Event::Key(key))) = maybe_key {
                        self.on_key(key);
                    }
                }
                Some(msg) = self.events_rx.recv() => self.on_agent_event(msg).await,
                Some(pending) = self.approvals_rx.recv() => {
                    self.pending = Some(pending);
                }
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Ctrl+C always quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // Approval modal captures input.
        if self.pending.is_some() {
            self.on_approval_key(key);
            return;
        }

        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Char(c) => {
                self.composer.push(c);
            }
            KeyCode::Backspace => {
                self.composer.pop();
            }
            KeyCode::Up => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
            }
            KeyCode::End => {
                self.follow = true;
            }
            _ => {}
        }
    }

    fn on_approval_key(&mut self, key: KeyEvent) {
        let decision = match key.code {
            KeyCode::Char('y') => Some(Decision::Approve),
            KeyCode::Char('a') => Some(Decision::ApproveForSession),
            KeyCode::Char('d') | KeyCode::Esc => Some(Decision::Deny),
            _ => None,
        };
        if let Some(decision) = decision {
            if let Some(pending) = self.pending.take() {
                let verb = if decision.is_approved() {
                    "approved"
                } else {
                    "denied"
                };
                self.transcript
                    .push(Entry::Info(format!("approval {verb}")));
                let _ = pending.respond.send(decision);
            }
        }
    }

    fn submit(&mut self) {
        let text = self.composer.trim().to_string();
        if text.is_empty() || self.running {
            return;
        }
        self.composer.clear();
        self.transcript.push(Entry::User(text.clone()));
        self.follow = true;

        // Move the session into a turn task; reclaim it on completion.
        let Some(mut session) = self.session.take() else {
            return;
        };
        self.running = true;
        let agent = Arc::clone(&self.agent);
        self.turn_handle = Some(tokio::spawn(async move {
            let mut rollout = None;
            agent.run_turn(&mut session, &text, &mut rollout).await;
            session
        }));
    }

    async fn on_agent_event(&mut self, msg: EventMsg) {
        match msg {
            EventMsg::AgentMessageDelta { delta } => {
                self.streaming
                    .get_or_insert_with(String::new)
                    .push_str(&delta);
            }
            EventMsg::AgentMessageDone { text } => {
                self.streaming = None;
                self.transcript.push(Entry::Assistant(text));
            }
            EventMsg::ToolCallBegin {
                name, args_preview, ..
            } => {
                self.transcript
                    .push(Entry::Tool(format!("{name} {args_preview}")));
            }
            EventMsg::ToolCallEnd { ok, summary, .. } => {
                self.transcript
                    .push(Entry::ToolResult { ok, text: summary });
            }
            EventMsg::Committed { sha, message } => {
                let short = &sha[..sha.len().min(8)];
                self.transcript
                    .push(Entry::Git(format!("committed {short}  {message}")));
            }
            EventMsg::Error { message, .. } => {
                self.transcript.push(Entry::Error(message));
            }
            EventMsg::TurnComplete { .. } => {
                if let Some(handle) = self.turn_handle.take() {
                    if let Ok(session) = handle.await {
                        self.session = Some(session);
                    }
                }
                self.running = false;
                self.streaming = None;
            }
            _ => {}
        }
        self.follow = true;
    }

    fn render(&self, f: &mut ratatui::Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(area);

        self.render_transcript(chunks[0], f);
        self.render_status(chunks[1], f);
        self.render_composer(chunks[2], f);

        if let Some(pending) = &self.pending {
            render_approval_modal(area, f, pending);
        }
    }

    fn render_transcript(&self, area: Rect, f: &mut ratatui::Frame) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        for entry in &self.transcript {
            push_entry_lines(&mut lines, entry);
        }
        if let Some(streaming) = &self.streaming {
            lines.push(Line::from(Span::styled(
                "grok",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )));
            for l in streaming.lines() {
                lines.push(Line::from(l.to_string()));
            }
        }

        let viewport = area.height.saturating_sub(2);
        let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let scroll = if self.follow {
            total.saturating_sub(viewport)
        } else {
            self.scroll.min(total.saturating_sub(1))
        };

        let para = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" conversation "),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        f.render_widget(para, area);
    }

    fn render_status(&self, area: Rect, f: &mut ratatui::Frame) {
        let indicator = if self.running {
            "● working"
        } else {
            "○ idle"
        };
        let text = format!(
            " {}  ·  {}  ·  {indicator}  ·  Ctrl+C quit ",
            self.status_model, self.status_preset
        );
        let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
        f.render_widget(para, area);
    }

    fn render_composer(&self, area: Rect, f: &mut ratatui::Frame) {
        let content = if self.composer.is_empty() {
            Span::styled(
                "Ask Grok…",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )
        } else {
            Span::raw(self.composer.as_str())
        };
        let para = Paragraph::new(Line::from(vec![Span::raw("› "), content]))
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
    }
}

fn push_entry_lines(lines: &mut Vec<Line<'static>>, entry: &Entry) {
    match entry {
        Entry::User(text) => {
            lines.push(Line::from(Span::styled(
                "you",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            for l in text.lines() {
                lines.push(Line::from(l.to_string()));
            }
        }
        Entry::Assistant(text) => {
            lines.push(Line::from(Span::styled(
                "grok",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )));
            for l in text.lines() {
                lines.push(Line::from(l.to_string()));
            }
        }
        Entry::Tool(text) => {
            lines.push(Line::from(Span::styled(
                format!("⚙ {text}"),
                Style::default().fg(Color::Yellow),
            )));
        }
        Entry::ToolResult { ok, text } => {
            let (glyph, color) = if *ok {
                ("✓", Color::Green)
            } else {
                ("✗", Color::Red)
            };
            lines.push(Line::from(Span::styled(
                format!("  {glyph} {text}"),
                Style::default().fg(color),
            )));
        }
        Entry::Git(text) => {
            lines.push(Line::from(Span::styled(
                format!("⎿ {text}"),
                Style::default().fg(Color::Blue),
            )));
        }
        Entry::Error(text) => {
            lines.push(Line::from(Span::styled(
                format!("error: {text}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
        }
        Entry::Info(text) => {
            lines.push(Line::from(Span::styled(
                text.clone(),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    lines.push(Line::from(""));
}

fn render_approval_modal(area: Rect, f: &mut ratatui::Frame, pending: &PendingApproval) {
    let modal = centered_rect(70, 40, area);
    f.render_widget(Clear, modal);

    let mut lines = vec![
        Line::from(Span::styled(
            format!("approval — {}", pending.request.reason),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("{:?}", pending.request.kind)),
        Line::from(""),
        Line::from(Span::styled(
            "[y] approve   [a] approve for session   [d] deny   Esc deny",
            Style::default().fg(Color::Yellow),
        )),
    ];
    // Truncate an over-long kind description so the modal doesn't overflow.
    if let Some(kind_line) = lines.get_mut(2) {
        let s = kind_line.to_string();
        if s.chars().count() > 200 {
            let truncated: String = s.chars().take(200).collect();
            *kind_line = Line::from(format!("{truncated}…"));
        }
    }

    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" approval required ")
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, modal);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::sync::Arc;

    use grokforge_core::{Agent, Session, SessionConfig, ToolRegistry};
    use grokforge_sandbox::PassthroughRunner;
    use grokforge_xai::XaiClient;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;

    use super::{App, Entry};
    use crate::approver::ChannelApprover;

    fn test_app() -> App {
        let client = XaiClient::new("http://127.0.0.1:1", "k").unwrap();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (approver, approvals_rx) = ChannelApprover::new();
        let agent = Arc::new(Agent::new(
            client,
            ToolRegistry::with_builtins(),
            Arc::new(PassthroughRunner),
            Arc::new(approver),
            events_tx,
        ));
        let session = Session::new(SessionConfig::new("/tmp".into(), "grok-build-0.1"));
        App::new(
            agent,
            session,
            events_rx,
            approvals_rx,
            "grok-build-0.1".to_string(),
            "auto".to_string(),
        )
    }

    fn buffer_text(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        buf.content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn renders_without_panicking_and_shows_chrome() {
        let app = test_app();
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("conversation"));
        assert!(text.contains("grok-build-0.1"));
        assert!(text.contains("Ask Grok"));
    }

    #[test]
    fn renders_a_transcript_entry() {
        let mut app = test_app();
        app.transcript.push(Entry::User("hello there".to_string()));
        app.transcript
            .push(Entry::Assistant("hi from grok".to_string()));
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("hello there"));
        assert!(text.contains("hi from grok"));
    }

    #[test]
    fn renders_approval_modal_when_pending() {
        use grokforge_protocol::{ApprovalId, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;

        let mut app = test_app();
        let (respond, _wait) = oneshot::channel();
        app.pending = Some(crate::approver::PendingApproval {
            request: ApprovalRequest {
                id: ApprovalId::new(),
                call_id: None,
                kind: ApprovalKind::WriteFile {
                    path: "/tmp/x".into(),
                },
                reason: "write a file".to_string(),
            },
            respond,
        });
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("approval required"));
        assert!(text.contains("approve"));
    }
}
