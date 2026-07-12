//! The interactive TUI application: an async event loop over terminal input, agent events, and
//! approval requests, rendering a scrolling transcript, a composer, a status line, and an
//! approval modal.
//!
//! This is the first working cut. It uses the alternate screen for robustness; the inline
//! viewport + native-scrollback render pipeline (the inline-scrollback differentiator in the design
//! docs) is the planned upgrade and slots in behind the same event/op flow.

use std::any::Any;
use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::{FutureExt, StreamExt};
use grokforge_core::{Agent, RolloutWriter, Session, TurnCancellation};
use grokforge_protocol::{Decision, EventMsg, ResponseItem};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthChar;

use crate::approver::PendingApproval;

const MAX_COMPOSER_BYTES: usize = 64 * 1024;
const MAX_TRANSCRIPT_ENTRIES: usize = 2_048;
const MAX_TRANSCRIPT_BYTES: usize = 2 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 48 * 1024;
const MAX_RENDER_TRANSCRIPT_ENTRIES: usize = 512;
const MAX_RENDER_TRANSCRIPT_BYTES: usize = 48 * 1024;
const APPROVAL_PAGE_BYTES: usize = 8 * 1024;
const REDRAW_INTERVAL: Duration = Duration::from_millis(33);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownState {
    Active,
    Requested,
    Ready,
}

/// Lazily paginates an approval across the actual modal viewport. Page boundaries are cached,
/// so ordinary redraws inspect at most [`APPROVAL_PAGE_BYTES`] instead of rescanning a large
/// command. The byte cap also bounds pathological zero-width Unicode input.
#[derive(Debug)]
struct ApprovalDetail {
    text: String,
    pager: RefCell<ApprovalPager>,
}

#[derive(Debug)]
struct ApprovalPager {
    width: usize,
    rows: usize,
    page_starts: Vec<usize>,
    current: usize,
    complete: bool,
}

#[derive(Debug)]
struct ApprovalPage {
    body: String,
    number: usize,
    total: Option<usize>,
    start: usize,
    end: usize,
    bytes: usize,
}

impl ApprovalDetail {
    fn new(text: String) -> Self {
        Self {
            text,
            // These defaults only matter if a key arrives before the first modal render. The
            // first render reflows around the actual viewport while preserving the byte anchor.
            pager: RefCell::new(ApprovalPager {
                width: 80,
                rows: 8,
                page_starts: vec![0],
                current: 0,
                complete: false,
            }),
        }
    }

    fn previous(&self, count: usize) {
        let mut pager = self.pager.borrow_mut();
        pager.current = pager.current.saturating_sub(count);
    }

    fn next(&self, count: usize) {
        let mut pager = self.pager.borrow_mut();
        for _ in 0..count {
            if pager.current + 1 < pager.page_starts.len() {
                pager.current += 1;
            } else if !self.extend_pages(&mut pager) {
                break;
            } else {
                pager.current += 1;
            }
        }
    }

    fn home(&self) {
        self.pager.borrow_mut().current = 0;
    }

    fn end(&self) {
        let mut pager = self.pager.borrow_mut();
        while self.extend_pages(&mut pager) {}
        pager.current = pager.page_starts.len().saturating_sub(1);
    }

    fn page(&self, width: u16, rows: u16) -> ApprovalPage {
        let width = usize::from(width.max(1));
        let rows = usize::from(rows.max(1));
        let mut pager = self.pager.borrow_mut();
        self.reflow_if_needed(&mut pager, width, rows);

        let page = pager.current.min(pager.page_starts.len().saturating_sub(1));
        pager.current = page;
        let start = pager.page_starts[page];
        let end = pager
            .page_starts
            .get(page + 1)
            .copied()
            .unwrap_or_else(|| approval_page_end(&self.text, start, pager.width, pager.rows));
        if end >= self.text.len() {
            pager.complete = true;
        }
        let total = pager.complete.then_some(pager.page_starts.len());
        ApprovalPage {
            body: approval_page_body(&self.text[start..end], pager.width),
            number: page + 1,
            total,
            start,
            end,
            bytes: self.text.len(),
        }
    }

    fn reflow_if_needed(&self, pager: &mut ApprovalPager, width: usize, rows: usize) {
        if pager.width == width && pager.rows == rows {
            return;
        }

        let anchor = pager.page_starts.get(pager.current).copied().unwrap_or(0);
        pager.width = width;
        pager.rows = rows;
        pager.page_starts.clear();
        pager.page_starts.push(0);
        pager.current = 0;
        pager.complete = self.text.is_empty();

        // A resize is the only operation that may rescan the prefix. Ordinary redraw and page
        // navigation remain bounded, and the currently viewed byte stays visible after reflow.
        while !pager.complete {
            let start = *pager.page_starts.last().unwrap_or(&0);
            let end = approval_page_end(&self.text, start, width, rows);
            if end > anchor || end >= self.text.len() {
                if end >= self.text.len() {
                    pager.complete = true;
                }
                break;
            }
            pager.page_starts.push(end);
            pager.current += 1;
        }
    }

    /// Extends the current layout by one page. Returns true only when a new page exists.
    fn extend_pages(&self, pager: &mut ApprovalPager) -> bool {
        if pager.complete {
            return false;
        }
        let start = *pager.page_starts.last().unwrap_or(&0);
        let end = approval_page_end(&self.text, start, pager.width, pager.rows);
        if end >= self.text.len() {
            pager.complete = true;
            return false;
        }
        pager.page_starts.push(end);
        true
    }
}

/// The running application state.
pub struct App {
    agent: Arc<Agent>,
    session: Option<Session>,
    rollout: Option<RolloutWriter>,
    transcript: Vec<Entry>,
    transcript_bytes: usize,
    omitted_entries: usize,
    streaming: Option<String>,
    composer: String,
    scroll: u16,
    follow: bool,
    pending: Option<PendingApproval>,
    /// Sanitized and cached once when the request arrives. Full approval details remain visible;
    /// redraws do not repeatedly format or scan a potentially large command.
    approval_detail: Option<ApprovalDetail>,
    running: bool,
    shutdown: ShutdownState,
    status_model: String,
    status_preset: String,

    // The channel stays open for the app's lifetime because `agent` (Arc) holds the sender.
    events_rx: mpsc::UnboundedReceiver<EventMsg>,
    approvals_rx: mpsc::UnboundedReceiver<PendingApproval>,
    turn_handle: Option<JoinHandle<TurnOutcome>>,
    turn_cancellation: Option<TurnCancellation>,
    turn_complete_seen: bool,
    undo_handle: Option<JoinHandle<Entry>>,
}

#[derive(Debug)]
struct TurnOutcome {
    session: Session,
    rollout: Option<RolloutWriter>,
    panic: Option<String>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("transcript_len", &self.transcript.len())
            .field("running", &self.running)
            .field("has_pending_approval", &self.pending.is_some())
            .field("has_pending_undo", &self.undo_handle.is_some())
            .finish_non_exhaustive()
    }
}

impl App {
    #[must_use]
    pub fn new(
        agent: Arc<Agent>,
        session: Session,
        rollout: Option<RolloutWriter>,
        events_rx: mpsc::UnboundedReceiver<EventMsg>,
        approvals_rx: mpsc::UnboundedReceiver<PendingApproval>,
        status_model: String,
        status_preset: String,
    ) -> Self {
        let resumed = session.history.len();
        let mut transcript = vec![Entry::Info(
            "GrokForge — type a message and press Enter. Ctrl+C to quit.".to_string(),
        )];
        let mut omitted_entries = 0;
        if resumed > 0 {
            transcript.push(Entry::Info(format!(
                "resumed session with {resumed} prior transcript item(s)"
            )));
            let (tail, omitted) = resumed_transcript_tail(&session.history);
            omitted_entries = omitted;
            transcript.extend(tail);
        }
        let transcript_bytes = transcript.iter().map(entry_bytes).sum();
        Self {
            agent,
            session: Some(session),
            rollout,
            transcript,
            transcript_bytes,
            omitted_entries,
            streaming: None,
            composer: String::new(),
            scroll: 0,
            follow: true,
            pending: None,
            approval_detail: None,
            running: false,
            shutdown: ShutdownState::Active,
            status_model,
            status_preset,
            events_rx,
            approvals_rx,
            turn_handle: None,
            turn_cancellation: None,
            turn_complete_seen: false,
            undo_handle: None,
        }
    }

    pub(crate) fn push_info(&mut self, message: impl Into<String>) {
        self.push_entry(Entry::Info(message.into()));
    }

    fn push_entry(&mut self, entry: Entry) {
        let entry = bounded_entry(entry);
        self.transcript_bytes = self.transcript_bytes.saturating_add(entry_bytes(&entry));
        self.transcript.push(entry);
        while self.transcript.len() > MAX_TRANSCRIPT_ENTRIES
            || self.transcript_bytes > MAX_TRANSCRIPT_BYTES
        {
            if self.transcript.is_empty() {
                break;
            }
            let removed = self.transcript.remove(0);
            self.transcript_bytes = self.transcript_bytes.saturating_sub(entry_bytes(&removed));
            self.omitted_entries = self.omitted_entries.saturating_add(1);
        }
    }

    fn request_quit(&mut self) {
        self.shutdown = ShutdownState::Requested;
        if let Some(cancellation) = &self.turn_cancellation {
            cancellation.cancel();
        }
        if let Some(pending) = self.pending.take() {
            let _ = pending.respond.send(Decision::Abort);
        }
        self.approval_detail = None;
        self.finish_quit_if_quiescent();
    }

    fn finish_quit_if_quiescent(&mut self) {
        if self.shutdown == ShutdownState::Requested
            && self.turn_handle.is_none()
            && self.undo_handle.is_none()
            && !self.running
        {
            self.shutdown = ShutdownState::Ready;
        }
    }

    async fn await_quiescence(&mut self) {
        if let Some(cancellation) = &self.turn_cancellation {
            cancellation.cancel();
        }
        if let Some(handle) = self.turn_handle.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.undo_handle.take() {
            let _ = handle.await;
        }
        self.running = false;
    }

    /// Run the event loop until the user quits.
    pub async fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<(), B::Error> {
        let mut input = EventStream::new();
        let mut redraw = tokio::time::interval(REDRAW_INTERVAL);
        redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut redraw_needed = true;
        while self.shutdown != ShutdownState::Ready {
            tokio::select! {
                _ = redraw.tick() => {
                    if redraw_needed {
                        if let Err(error) = terminal.draw(|f| self.render(f)) {
                            self.request_quit();
                            self.await_quiescence().await;
                            return Err(error);
                        }
                        redraw_needed = false;
                    }
                }
                maybe_event = input.next() => {
                    match maybe_event {
                        Some(Ok(Event::Key(key))) => self.on_terminal_key(key),
                        Some(Ok(Event::Paste(text))) => self.on_paste(&text),
                        Some(Ok(_)) => {}
                        Some(Err(error)) => {
                            self.push_entry(Entry::Error(format!("terminal input failed: {error}")));
                            self.request_quit();
                        }
                        None => {
                            self.push_entry(Entry::Error("terminal input stream closed".to_string()));
                            self.request_quit();
                        }
                    }
                    redraw_needed = true;
                }
                Some(msg) = self.events_rx.recv() => {
                    self.on_agent_event(msg);
                    redraw_needed = true;
                }
                Some(pending) = self.approvals_rx.recv() => {
                    self.on_approval_request(pending);
                    redraw_needed = true;
                }
                result = wait_for_turn(&mut self.turn_handle), if self.turn_handle.is_some() => {
                    self.finish_turn(result);
                    redraw_needed = true;
                }
                result = wait_for_undo(&mut self.undo_handle), if self.undo_handle.is_some() => {
                    self.finish_undo(result);
                    redraw_needed = true;
                }
            }
        }
        self.await_quiescence().await;
        Ok(())
    }

    fn on_terminal_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Release {
            self.on_key(key);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Ctrl+C always quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }

        // Approval modal captures input.
        if self.pending.is_some() {
            self.on_approval_key(key);
            return;
        }

        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Char(c)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL
                        | KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::HYPER
                        | KeyModifiers::META,
                ) =>
            {
                if self.composer.len().saturating_add(c.len_utf8()) <= MAX_COMPOSER_BYTES {
                    self.composer.push(c);
                }
            }
            KeyCode::Backspace => {
                self.composer.pop();
            }
            KeyCode::Up => {
                self.follow = false;
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_sub(1);
                if self.scroll == 0 {
                    self.follow = true;
                }
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll = self.scroll.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                if self.scroll == 0 {
                    self.follow = true;
                }
            }
            KeyCode::End => {
                self.follow = true;
                self.scroll = 0;
            }
            _ => {}
        }
    }

    fn on_paste(&mut self, value: &str) {
        if self.pending.is_some() || self.running {
            return;
        }
        let value = safe_terminal_text(value);
        let remaining = MAX_COMPOSER_BYTES.saturating_sub(self.composer.len());
        let mut used = 0usize;
        self.composer.extend(value.chars().take_while(|ch| {
            used = used.saturating_add(ch.len_utf8());
            used <= remaining
        }));
    }

    fn on_approval_request(&mut self, pending: PendingApproval) {
        // Ctrl+C can race with an approval being emitted. Never leave the turn blocked on a
        // request that arrived after shutdown started.
        if self.shutdown != ShutdownState::Active {
            let _ = pending.respond.send(Decision::Abort);
            return;
        }
        self.approval_detail = Some(ApprovalDetail::new(safe_terminal_text(&format!(
            "reason: {}\nrequest: {:?}",
            pending.request.reason, pending.request.kind
        ))));
        self.pending = Some(pending);
    }

    fn on_approval_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if let Some(detail) = &self.approval_detail {
                    detail.previous(1);
                }
                return;
            }
            KeyCode::Down => {
                if let Some(detail) = &self.approval_detail {
                    detail.next(1);
                }
                return;
            }
            KeyCode::PageUp => {
                if let Some(detail) = &self.approval_detail {
                    detail.previous(10);
                }
                return;
            }
            KeyCode::PageDown => {
                if let Some(detail) = &self.approval_detail {
                    detail.next(10);
                }
                return;
            }
            KeyCode::Home => {
                if let Some(detail) = &self.approval_detail {
                    detail.home();
                }
                return;
            }
            KeyCode::End => {
                if let Some(detail) = &self.approval_detail {
                    detail.end();
                }
                return;
            }
            _ => {}
        }
        let decision = match key.code {
            KeyCode::Char('y') => Some(Decision::Approve),
            KeyCode::Char('a') => Some(Decision::ApproveForSession),
            KeyCode::Char('d') | KeyCode::Esc => Some(Decision::Deny),
            _ => None,
        };
        if let Some(decision) = decision
            && let Some(pending) = self.pending.take()
        {
            self.approval_detail = None;
            let verb = if decision.is_approved() {
                "approved"
            } else {
                "denied"
            };
            self.push_entry(Entry::Info(format!("approval {verb}")));
            let _ = pending.respond.send(decision);
        }
    }

    fn submit(&mut self) {
        let text = self.composer.trim().to_string();
        if text.is_empty() || self.running {
            return;
        }
        self.composer.clear();
        if let Some(cmd) = text.strip_prefix('/') {
            self.handle_slash(cmd);
            return;
        }
        self.push_entry(Entry::User(text.clone()));
        self.follow = true;
        self.start_turn(text, false);
    }

    /// Spawn a turn (execute or plan mode), moving the session + rollout into the task.
    fn start_turn(&mut self, text: String, plan: bool) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        let mut rollout = self.rollout.take();
        self.running = true;
        self.turn_complete_seen = false;
        let cancellation = TurnCancellation::new();
        self.turn_cancellation = Some(cancellation.clone());
        let agent = Arc::clone(&self.agent);
        self.turn_handle = Some(tokio::spawn(async move {
            let turn = AssertUnwindSafe(async {
                if plan {
                    agent
                        .run_plan_turn_cancellable(&mut session, &text, &mut rollout, &cancellation)
                        .await;
                } else {
                    agent
                        .run_turn_cancellable(&mut session, &text, &mut rollout, &cancellation)
                        .await;
                }
            })
            .catch_unwind()
            .await;
            TurnOutcome {
                session,
                rollout,
                panic: turn.err().map(|payload| panic_message(payload.as_ref())),
            }
        }));
    }

    fn handle_slash(&mut self, cmd: &str) {
        let (name, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
        match name {
            "help" | "?" => {
                self.push_entry(Entry::Info(
                    "commands: /plan <task>  ·  /undo  ·  /clear  ·  /help  ·  /quit".to_string(),
                ));
            }
            "quit" | "exit" | "q" => self.request_quit(),
            "clear" => {
                self.transcript.clear();
                self.transcript_bytes = 0;
                self.omitted_entries = 0;
            }
            "undo" => self.undo(),
            "plan" => {
                let task = rest.trim();
                if task.is_empty() {
                    self.push_entry(Entry::Info("usage: /plan <task>".to_string()));
                } else {
                    self.push_entry(Entry::User(format!("/plan {task}")));
                    self.follow = true;
                    self.start_turn(task.to_string(), true);
                }
            }
            other => {
                self.push_entry(Entry::Info(format!(
                    "unknown command: /{other} (try /help)"
                )));
            }
        }
    }

    /// Undo the last agent commit for this session (git, from the host process).
    fn undo(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let root = session.config.workspace_root.clone();
        let id = session.id;
        self.follow = true;
        self.scroll = 0;
        self.running = true;
        self.undo_handle =
            Some(tokio::task::spawn_blocking(
                move || match grokforge_git::Git::discover(&root) {
                    Some(git) => match git.undo_last(id) {
                        Ok(Some(msg)) => Entry::Git(format!("undo: {msg}")),
                        Ok(None) => Entry::Info("nothing to undo for this session".to_string()),
                        Err(error) => Entry::Error(format!("undo failed: {error}")),
                    },
                    None => Entry::Info("not a git repository".to_string()),
                },
            ));
    }

    fn on_agent_event(&mut self, msg: EventMsg) {
        match msg {
            EventMsg::AgentMessageDelta { delta } => {
                append_bounded(
                    self.streaming.get_or_insert_with(String::new),
                    &delta,
                    MAX_ENTRY_BYTES,
                );
            }
            EventMsg::AgentMessageDone { text } => {
                self.streaming = None;
                self.push_entry(Entry::Assistant(text));
            }
            EventMsg::ToolCallBegin {
                name, args_preview, ..
            } => {
                self.push_entry(Entry::Tool(format!("{name} {args_preview}")));
            }
            EventMsg::ToolCallEnd { ok, summary, .. } => {
                self.push_entry(Entry::ToolResult { ok, text: summary });
            }
            EventMsg::Committed { sha, message } => {
                let short = &sha[..sha.len().min(8)];
                self.push_entry(Entry::Git(format!("committed {short}  {message}")));
            }
            EventMsg::Error { message, .. } => {
                self.push_entry(Entry::Error(message));
            }
            EventMsg::TurnComplete { .. } => {
                // All earlier events from this turn are ahead of TurnComplete in the FIFO. Only
                // advertise idle after both this marker and the task join have been observed.
                self.turn_complete_seen = true;
                if self.turn_handle.is_none() {
                    self.running = false;
                    self.turn_cancellation = None;
                    self.finish_quit_if_quiescent();
                }
            }
            _ => {}
        }
    }

    fn finish_turn(&mut self, result: Result<TurnOutcome, tokio::task::JoinError>) {
        self.turn_handle.take();
        self.streaming = None;
        match result {
            Ok(outcome) => {
                self.session = Some(outcome.session);
                self.rollout = outcome.rollout;
                if let Some(message) = outcome.panic {
                    self.push_entry(Entry::Error(format!(
                        "turn task panicked: {message}; session closed so recovery can repair any interrupted tool call"
                    )));
                    // Continuing with the in-memory transcript could replay a durably recorded
                    // function call whose output was lost in the panic. Closing releases the
                    // rollout lock; the next `resume` atomically repairs outstanding calls.
                    self.running = false;
                    self.turn_cancellation = None;
                    self.request_quit();
                } else if self.turn_complete_seen {
                    self.running = false;
                    self.turn_cancellation = None;
                }
            }
            Err(error) => {
                // A cancelled join loses ownership of the moved session; exit cleanly rather
                // than trapping the UI forever in a fake "working" state.
                self.push_entry(Entry::Error(format!("turn task failed: {error}")));
                self.running = false;
                self.turn_cancellation = None;
                self.request_quit();
            }
        }
        self.finish_quit_if_quiescent();
    }

    fn finish_undo(&mut self, result: Result<Entry, tokio::task::JoinError>) {
        self.undo_handle.take();
        self.running = false;
        match result {
            Ok(entry) => self.push_entry(entry),
            Err(error) => self.push_entry(Entry::Error(format!("undo task failed: {error}"))),
        }
        self.finish_quit_if_quiescent();
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

        if let (Some(pending), Some(detail)) = (&self.pending, &self.approval_detail) {
            render_approval_modal(area, f, pending, detail);
        }
    }

    fn render_transcript(&self, area: Rect, f: &mut ratatui::Frame) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        // Keep redraw work and Paragraph's u16 scroll range bounded even when the retained
        // transcript is several MiB. The durable rollout still contains the complete history.
        let streaming_bytes = self.streaming.as_ref().map_or(0, String::len);
        let mut byte_budget = MAX_RENDER_TRANSCRIPT_BYTES
            .saturating_sub(streaming_bytes.min(MAX_RENDER_TRANSCRIPT_BYTES));
        let mut visible_start = self.transcript.len();
        let mut visible_entries = 0usize;
        while visible_start > 0 && visible_entries < MAX_RENDER_TRANSCRIPT_ENTRIES {
            let candidate = visible_start - 1;
            let bytes = entry_bytes(&self.transcript[candidate]);
            if bytes > byte_budget {
                break;
            }
            byte_budget -= bytes;
            visible_start = candidate;
            visible_entries += 1;
        }
        let render_omitted = self.omitted_entries.saturating_add(visible_start);
        if render_omitted > 0 {
            push_entry_lines(
                &mut lines,
                &Entry::Info(format!(
                    "… {render_omitted} earlier transcript item(s) omitted from this view; full history remains in the session rollout …"
                )),
            );
        }
        for entry in &self.transcript[visible_start..] {
            push_entry_lines(&mut lines, entry);
        }
        if let Some(streaming) = &self.streaming {
            lines.push(Line::from(Span::styled(
                "grok",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )));
            let streaming = safe_terminal_text(streaming);
            for l in streaming.lines() {
                lines.push(Line::from(l.to_string()));
            }
        }

        let viewport = area.height.saturating_sub(2);
        let content_width = area.width.saturating_sub(2).max(1);
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = u16::try_from(para.line_count(content_width)).unwrap_or(u16::MAX);
        let max_scroll = total.saturating_sub(viewport);
        let scroll = if self.follow {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll)
        };

        let para = para
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" conversation "),
            )
            .scroll((scroll, 0));
        f.render_widget(para, area);
    }

    fn render_status(&self, area: Rect, f: &mut ratatui::Frame) {
        let indicator = if self.running {
            "● working"
        } else {
            "○ idle"
        };
        let hidden = if self.omitted_entries > 0 {
            format!("  ·  {} history item(s) hidden", self.omitted_entries)
        } else {
            String::new()
        };
        let text = format!(
            " {}  ·  {}  ·  {indicator}{hidden}  ·  Ctrl+C quit ",
            safe_terminal_line(&self.status_model),
            safe_terminal_line(&self.status_preset)
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
        if self.pending.is_none() && area.width > 2 && area.height > 2 {
            let prefix_width = 2u16;
            let composer_width =
                u16::try_from(Line::from(self.composer.as_str()).width()).unwrap_or(u16::MAX);
            let max_x = area.right().saturating_sub(2);
            let x = area
                .x
                .saturating_add(1)
                .saturating_add(prefix_width)
                .saturating_add(composer_width)
                .min(max_x);
            f.set_cursor_position((x, area.y.saturating_add(1)));
        }
    }
}

async fn wait_for_turn(
    handle: &mut Option<JoinHandle<TurnOutcome>>,
) -> Result<TurnOutcome, tokio::task::JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

async fn wait_for_undo(
    handle: &mut Option<JoinHandle<Entry>>,
) -> Result<Entry, tokio::task::JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn resumed_transcript_tail(history: &[ResponseItem]) -> (Vec<Entry>, usize) {
    let mut tail = Vec::new();
    let mut bytes = 0usize;
    let entry_budget = MAX_TRANSCRIPT_ENTRIES.saturating_sub(2);
    let byte_budget = MAX_TRANSCRIPT_BYTES.saturating_sub(64 * 1024);
    for (scanned, item) in history.iter().rev().enumerate() {
        if scanned >= MAX_TRANSCRIPT_ENTRIES.saturating_mul(4)
            || tail.len() >= entry_budget
            || bytes >= byte_budget
        {
            break;
        }
        let Some(entry) = entry_from_history(item).map(bounded_entry) else {
            continue;
        };
        let entry_len = entry_bytes(&entry);
        if !tail.is_empty() && bytes.saturating_add(entry_len) > byte_budget {
            break;
        }
        bytes = bytes.saturating_add(entry_len);
        tail.push(entry);
    }
    tail.reverse();
    let omitted = history.len().saturating_sub(tail.len());
    (tail, omitted)
}

fn entry_bytes(entry: &Entry) -> usize {
    match entry {
        Entry::User(text)
        | Entry::Assistant(text)
        | Entry::Tool(text)
        | Entry::Git(text)
        | Entry::Error(text)
        | Entry::Info(text)
        | Entry::ToolResult { text, .. } => text.len(),
    }
}

fn bounded_entry(entry: Entry) -> Entry {
    match entry {
        Entry::User(text) => Entry::User(bounded_text(&text, MAX_ENTRY_BYTES)),
        Entry::Assistant(text) => Entry::Assistant(bounded_text(&text, MAX_ENTRY_BYTES)),
        Entry::Tool(text) => Entry::Tool(bounded_text(&text, MAX_ENTRY_BYTES)),
        Entry::ToolResult { ok, text } => Entry::ToolResult {
            ok,
            text: bounded_text(&text, MAX_ENTRY_BYTES),
        },
        Entry::Git(text) => Entry::Git(bounded_text(&text, MAX_ENTRY_BYTES)),
        Entry::Error(text) => Entry::Error(bounded_text(&text, MAX_ENTRY_BYTES)),
        Entry::Info(text) => Entry::Info(bounded_text(&text, MAX_ENTRY_BYTES)),
    }
}

fn bounded_text(value: &str, cap: usize) -> String {
    if value.len() <= cap {
        return value.to_string();
    }
    let marker = "\n… [entry truncated in TUI; full value remains in rollout] …\n";
    let payload = cap.saturating_sub(marker.len());
    let mut head_end = payload / 2;
    while head_end > 0 && !value.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = value.len().saturating_sub(payload.saturating_sub(head_end));
    while tail_start < value.len() && !value.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}{marker}{}", &value[..head_end], &value[tail_start..])
}

fn append_bounded(target: &mut String, delta: &str, cap: usize) {
    if target.len() >= cap {
        return;
    }
    let remaining = cap.saturating_sub(target.len());
    if delta.len() <= remaining {
        target.push_str(delta);
        return;
    }
    let mut end = remaining;
    while end > 0 && !delta.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&delta[..end]);
    target.push_str("\n… [streaming display truncated; final value is retained in rollout] …");
}

fn entry_from_history(item: &ResponseItem) -> Option<Entry> {
    match item {
        ResponseItem::UserMessage { text, .. } => {
            Some(Entry::User(bounded_text(text, MAX_ENTRY_BYTES)))
        }
        ResponseItem::AssistantMessage { text } => {
            Some(Entry::Assistant(bounded_text(text, MAX_ENTRY_BYTES)))
        }
        ResponseItem::Reasoning { text } => Some(Entry::Info(format!(
            "thinking: {}",
            bounded_text(text, MAX_ENTRY_BYTES.saturating_sub(10))
        ))),
        ResponseItem::ToolCall {
            name, arguments, ..
        } => Some(Entry::Tool(format!(
            "{} {}",
            bounded_text(name, 1_024),
            bounded_text(arguments, MAX_ENTRY_BYTES.saturating_sub(1_025))
        ))),
        ResponseItem::ToolResult {
            content, is_error, ..
        } => Some(Entry::ToolResult {
            ok: !is_error,
            text: bounded_text(content, MAX_ENTRY_BYTES),
        }),
        ResponseItem::CompactionSummary { text, .. } => Some(Entry::Info(format!(
            "compacted history: {}",
            bounded_text(text, MAX_ENTRY_BYTES.saturating_sub(20))
        ))),
        ResponseItem::ProviderOutput { item } => entry_from_provider_output(item),
        ResponseItem::EncryptedReasoning { .. } | ResponseItem::CompactionCheckpoint { .. } => None,
    }
}

fn entry_from_provider_output(item: &serde_json::Value) -> Option<Entry> {
    match item.get("type").and_then(serde_json::Value::as_str) {
        Some("message") => {
            let mut text = String::new();
            for part in item
                .get("content")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter(|part| {
                    part.get("type").and_then(serde_json::Value::as_str) == Some("output_text")
                })
            {
                if let Some(delta) = part.get("text").and_then(serde_json::Value::as_str) {
                    append_bounded(&mut text, delta, MAX_ENTRY_BYTES);
                }
            }
            (!text.is_empty()).then_some(Entry::Assistant(text))
        }
        Some("function_call") => {
            let name = item
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<unknown>");
            let arguments = item
                .get("arguments")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("{}");
            Some(Entry::Tool(format!(
                "{} {}",
                bounded_text(name, 1_024),
                bounded_text(arguments, MAX_ENTRY_BYTES.saturating_sub(1_025))
            )))
        }
        Some(kind) if kind.ends_with("_call") => Some(Entry::Tool(format!(
            "{kind} [provider details retained in rollout]"
        ))),
        _ => None,
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
            let text = safe_terminal_text(text);
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
            let text = safe_terminal_text(text);
            for l in text.lines() {
                lines.push(Line::from(l.to_string()));
            }
        }
        Entry::Tool(text) => {
            lines.push(Line::from(Span::styled(
                format!("⚙ {}", safe_terminal_text(text)),
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
                format!("  {glyph} {}", safe_terminal_text(text)),
                Style::default().fg(color),
            )));
        }
        Entry::Git(text) => {
            lines.push(Line::from(Span::styled(
                format!("⎿ {}", safe_terminal_text(text)),
                Style::default().fg(Color::Blue),
            )));
        }
        Entry::Error(text) => {
            lines.push(Line::from(Span::styled(
                format!("error: {}", safe_terminal_text(text)),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
        }
        Entry::Info(text) => {
            lines.push(Line::from(Span::styled(
                safe_terminal_text(text),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    lines.push(Line::from(""));
}

fn render_approval_modal(
    area: Rect,
    f: &mut ratatui::Frame,
    pending: &PendingApproval,
    detail: &ApprovalDetail,
) {
    let modal = centered_rect(70, 40, area);
    f.render_widget(Clear, modal);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" approval required ")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inner);
    let detail_rows = chunks[2].height.max(1);
    let page = detail.page(chunks[2].width, detail_rows);
    let total = page
        .total
        .map_or_else(|| "?".to_string(), |total| total.to_string());

    let reason = Paragraph::new(Line::from(Span::styled(
        format!("approval — {}", safe_terminal_line(&pending.request.reason)),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    f.render_widget(reason, chunks[0]);

    let status = Paragraph::new(format!(
        "detail page {}/{total} · bytes {}..{} / {}",
        page.number, page.start, page.end, page.bytes
    ))
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(status, chunks[1]);

    let body = Paragraph::new(page.body).wrap(Wrap { trim: false });
    f.render_widget(body, chunks[2]);

    let controls = Paragraph::new(vec![
        Line::from("↑/↓ page · PgUp/PgDn ×10 · Home/End"),
        Line::from("y approve · a approve for session · d/Esc deny"),
    ])
    .style(Style::default().fg(Color::Yellow));
    f.render_widget(controls, chunks[3]);
}

/// Returns an exclusive UTF-8 byte boundary for one viewport-sized page.
fn approval_page_end(text: &str, start: usize, width: usize, rows: usize) -> usize {
    if start >= text.len() {
        return text.len();
    }

    let width = width.max(1);
    let rows = rows.max(1);
    let mut byte_limit = start.saturating_add(APPROVAL_PAGE_BYTES).min(text.len());
    while byte_limit > start && !text.is_char_boundary(byte_limit) {
        byte_limit -= 1;
    }

    let mut end = start;
    let mut row = 0usize;
    let mut column = 0usize;
    for (offset, ch) in text[start..byte_limit].char_indices() {
        let next = start + offset + ch.len_utf8();
        if ch == '\n' {
            end = next;
            row += 1;
            column = 0;
            if row >= rows {
                break;
            }
            continue;
        }

        let char_width = approval_char_width(ch);
        if column > 0 && column.saturating_add(char_width) > width {
            row += 1;
            if row >= rows {
                break;
            }
            column = 0;
        }
        end = next;
        column = column.saturating_add(char_width);
    }

    if end == start {
        // A one-column viewport can still consume a double-width character, and a very small
        // byte cap can still land before a multi-byte scalar. Always make forward progress.
        if let Some(ch) = text[start..].chars().next() {
            return start.saturating_add(ch.len_utf8()).min(text.len());
        }
    }
    end
}

fn approval_page_body(page: &str, width: usize) -> String {
    let width = width.max(1);
    let mut rendered = String::with_capacity(page.len());
    let mut column = 0usize;
    for (offset, ch) in page.char_indices() {
        if ch == '\n' {
            // A trailing newline was consumed as part of this page boundary. It should not add
            // an inaccessible blank display row below the viewport.
            if offset + ch.len_utf8() < page.len() {
                rendered.push('\n');
            }
            column = 0;
            continue;
        }

        let char_width = approval_char_width(ch);
        if column > 0 && column.saturating_add(char_width) > width {
            rendered.push('\n');
            column = 0;
        }
        if ch == '\t' {
            rendered.push_str("    ");
        } else {
            rendered.push(ch);
        }
        column = column.saturating_add(char_width);
    }
    rendered
}

fn approval_char_width(ch: char) -> usize {
    if ch == '\t' {
        4
    } else {
        ch.width().unwrap_or(0)
    }
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

fn safe_terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|ch| {
            matches!(ch, '\n' | '\t')
                || (!ch.is_control() && !matches!(*ch as u32, 0x7f..=0x9f) && !is_bidi_control(*ch))
        })
        .collect()
}

fn safe_terminal_line(value: &str) -> String {
    safe_terminal_text(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch as u32,
        0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::fmt::Write as _;
    use std::sync::Arc;

    use grokforge_core::{Agent, Session, SessionConfig, ToolRegistry};
    use grokforge_protocol::{Decision, EventMsg, ResponseItem};
    use grokforge_sandbox::PassthroughRunner;
    use grokforge_xai::XaiClient;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;
    use tokio::sync::mpsc;

    use super::{App, ApprovalDetail, Entry, MAX_COMPOSER_BYTES, TurnOutcome};
    use crate::approver::ChannelApprover;

    fn test_app() -> App {
        test_app_with_history(Vec::new())
    }

    fn test_app_with_history(history: Vec<ResponseItem>) -> App {
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
        let session =
            Session::with_history(SessionConfig::new("/tmp".into(), "grok-build-0.1"), history);
        App::new(
            agent,
            session,
            None,
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
        app.approval_detail = Some(ApprovalDetail::new(
            "reason: write a file\nrequest: WriteFile { path: /tmp/x }".to_string(),
        ));
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("approval required"));
        assert!(text.contains("approve"));
    }

    #[test]
    fn approval_pager_reaches_every_byte_across_many_lines() {
        let mut text = String::new();
        for index in 0..2_000 {
            writeln!(text, "line-{index:04} — approval context").expect("write string");
        }
        let detail = ApprovalDetail::new(text.clone());
        let mut expected_start = 0usize;
        let mut pages = 0usize;
        loop {
            let page = detail.page(24, 4);
            assert_eq!(page.start, expected_start);
            assert!(page.end > page.start || text.is_empty());
            assert!(page.body.lines().count() <= 4);
            for line in page.body.lines() {
                assert!(Line::from(line).width() <= 24);
            }
            pages += 1;
            if page.end == text.len() {
                assert_eq!(page.total, Some(pages));
                break;
            }
            expected_start = page.end;
            detail.next(1);
        }
        assert!(pages > 100);
    }

    #[test]
    fn approval_pager_hard_wraps_long_lines_and_end_reaches_the_tail() {
        let marker = "TAIL-APPROVAL-MARKER";
        let text = format!("{}\n{marker}", "界abcdef".repeat(20_000));
        let detail = ApprovalDetail::new(text.clone());

        let first = detail.page(17, 3);
        assert!(first.end <= super::APPROVAL_PAGE_BYTES);
        assert!(first.body.lines().count() <= 3);
        assert!(detail.pager.borrow().page_starts.len() <= 1);

        detail.end();
        let last = detail.page(17, 3);
        assert_eq!(last.end, text.len());
        assert!(
            last.body.replace('\n', "").contains(marker),
            "last page {}..{} of {} was {:?}",
            last.start,
            last.end,
            text.len(),
            last.body
        );
        assert!(last.total.is_some());
        for line in last.body.lines() {
            assert!(Line::from(line).width() <= 17);
        }
    }

    #[test]
    fn resumed_history_is_visible_in_the_transcript() {
        let app = test_app_with_history(vec![
            ResponseItem::user("prior question"),
            ResponseItem::assistant("prior answer"),
            ResponseItem::ProviderOutput {
                item: serde_json::json!({
                    "type": "message",
                    "content": [{"type": "output_text", "text": "raw assistant"}]
                }),
            },
            ResponseItem::ProviderOutput {
                item: serde_json::json!({
                    "type": "function_call",
                    "name": "read_file",
                    "arguments": "{\"path\":\"a.rs\"}"
                }),
            },
        ]);
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("prior question"));
        assert!(text.contains("prior answer"));
        assert!(text.contains("raw assistant"));
        assert!(text.contains("read_file"));
    }

    #[test]
    fn resumed_history_materializes_only_a_bounded_recent_tail() {
        let history = (0..10_000)
            .map(|index| ResponseItem::assistant(format!("history-{index}")))
            .collect();
        let app = test_app_with_history(history);
        assert!(app.transcript.len() <= super::MAX_TRANSCRIPT_ENTRIES);
        assert!(app.transcript_bytes <= super::MAX_TRANSCRIPT_BYTES);
        assert!(app.omitted_entries > 0);
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("hidden"));
        assert!(text.contains("history-9999"));
    }

    #[test]
    fn a_huge_resumed_entry_is_bounded_without_losing_its_tail() {
        let marker = "RECENT-TAIL";
        let app = test_app_with_history(vec![ResponseItem::assistant(format!(
            "{}{marker}",
            "x".repeat(super::MAX_ENTRY_BYTES * 4)
        ))]);
        let assistant = app
            .transcript
            .iter()
            .find_map(|entry| match entry {
                Entry::Assistant(text) => Some(text),
                _ => None,
            })
            .expect("assistant entry");
        assert!(assistant.len() <= super::MAX_ENTRY_BYTES);
        assert!(assistant.contains("entry truncated in TUI"));
        assert!(assistant.contains(marker));
    }

    #[tokio::test]
    async fn idle_waits_for_both_turn_complete_and_join() {
        use grokforge_protocol::{StopReason, TurnId};

        let mut app = test_app();
        let session = app.session.take().expect("session");
        app.running = true;
        app.turn_handle = Some(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            TurnOutcome {
                session,
                rollout: None,
                panic: None,
            }
        }));
        app.on_agent_event(EventMsg::TurnComplete {
            turn_id: TurnId::new(),
            stop: StopReason::EndTurn,
        });
        assert!(app.running, "join has not completed yet");
        let result = super::wait_for_turn(&mut app.turn_handle).await;
        app.finish_turn(result);
        assert!(!app.running);

        let mut app = test_app();
        let session = app.session.take().expect("session");
        app.running = true;
        app.turn_handle = Some(tokio::spawn(async move {
            TurnOutcome {
                session,
                rollout: None,
                panic: None,
            }
        }));
        let result = super::wait_for_turn(&mut app.turn_handle).await;
        app.finish_turn(result);
        assert!(app.running, "TurnComplete is still queued");
        app.on_agent_event(EventMsg::TurnComplete {
            turn_id: TurnId::new(),
            stop: StopReason::EndTurn,
        });
        assert!(!app.running);
    }

    #[test]
    fn up_from_follow_mode_scrolls_one_row_instead_of_jumping_to_top() {
        use crossterm::event::{KeyCode, KeyEvent};

        let mut app = test_app();
        for index in 0..50 {
            app.transcript
                .push(Entry::Assistant(format!("message-{index}")));
        }
        app.on_key(KeyEvent::from(KeyCode::Up));
        let text = buffer_text(&app, 60, 12);
        assert!(text.contains("message-49"));
        assert!(!text.contains("GrokForge — type"));
    }

    #[test]
    fn follow_mode_accounts_for_wrapped_rows_and_keeps_latest_visible() {
        let mut app = test_app();
        app.transcript.push(Entry::Assistant("word ".repeat(300)));
        app.transcript
            .push(Entry::Assistant("LATEST-MARKER".to_string()));
        let text = buffer_text(&app, 40, 12);
        assert!(text.contains("LATEST-MARKER"));
    }

    #[test]
    fn streaming_events_do_not_override_manual_scroll() {
        let mut app = test_app();
        app.follow = false;
        app.scroll = 12;
        app.on_agent_event(EventMsg::AgentMessageDelta {
            delta: "new streaming text".to_string(),
        });
        app.on_agent_event(EventMsg::ToolCallEnd {
            call_id: grokforge_protocol::ToolCallId::new(),
            ok: true,
            summary: "done".to_string(),
            denial: None,
        });
        assert!(!app.follow);
        assert_eq!(app.scroll, 12);
    }

    #[test]
    fn render_window_stays_bounded_and_keeps_the_latest_entry_visible() {
        let mut app = test_app();
        for index in 0..2_000 {
            app.push_entry(Entry::Assistant(format!(
                "entry-{index}: {}",
                "x".repeat(1_000)
            )));
        }
        app.push_entry(Entry::Assistant("LATESTQ".to_string()));

        let text = buffer_text(&app, 3, 12);
        assert!(text.contains('Q'));
        assert!(app.omitted_entries > 0 || app.transcript.len() > 100);
    }

    #[test]
    fn key_release_events_can_be_identified_and_ignored_by_the_loop() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let event = KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        let mut app = test_app();
        app.on_terminal_key(event);
        assert!(app.composer.is_empty());
    }

    #[test]
    fn modified_chars_are_not_inserted_and_paste_is_bounded() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = test_app();
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        assert!(app.composer.is_empty());

        app.on_paste(&format!(
            "{}\u{1b}\u{202e}",
            "a".repeat(MAX_COMPOSER_BYTES + 100)
        ));
        assert_eq!(app.composer.len(), MAX_COMPOSER_BYTES);
        assert!(!app.composer.contains('\u{1b}'));
        assert!(!app.composer.contains('\u{202e}'));
    }

    #[tokio::test]
    async fn ctrl_c_requests_cancellation_but_awaits_active_turn_quiescence() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = test_app();
        let session = app.session.take().expect("session");
        app.running = true;
        let cancellation = grokforge_core::TurnCancellation::new();
        app.turn_cancellation = Some(cancellation.clone());
        app.turn_handle = Some(tokio::spawn(async move {
            // Simulates an already-running host mutation, which must not be detached/aborted.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            TurnOutcome {
                session,
                rollout: None,
                panic: None,
            }
        }));
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.shutdown, super::ShutdownState::Requested);
        assert!(cancellation.is_cancelled());
        let result = super::wait_for_turn(&mut app.turn_handle).await;
        app.turn_complete_seen = true;
        app.finish_turn(result);
        assert_eq!(app.shutdown, super::ShutdownState::Ready);
    }

    #[tokio::test]
    async fn approval_arriving_after_shutdown_is_aborted_without_blocking_the_turn() {
        use grokforge_protocol::{ApprovalId, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;

        let mut app = test_app();
        app.request_quit();
        let (respond, wait) = oneshot::channel();
        app.on_approval_request(crate::approver::PendingApproval {
            request: ApprovalRequest {
                id: ApprovalId::new(),
                call_id: None,
                kind: ApprovalKind::WriteFile {
                    path: "/tmp/late".into(),
                },
                reason: "late request".to_string(),
            },
            respond,
        });

        assert_eq!(wait.await.expect("decision"), Decision::Abort);
        assert!(app.pending.is_none());
    }
}
